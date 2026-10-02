/* SPDX-License-Identifier: MIT */
/* Actual retained-copy callback and manifest/fragment consumers. These are
 * deterministic C boundary controls, not kernel/BPF payload qualification. */
#include <assert.h>
#include <errno.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "fd-effects.h"
enum ap_slot_state { AP_SLOT_FREE, AP_SLOT_RESERVED, AP_SLOT_ACTIVE, AP_SLOT_DISARMING, AP_SLOT_COLLECTED, AP_SLOT_QUARANTINED };
struct ap_pending_command {
    enum ap_slot_state state;
    struct ap_task_command submitted;
    struct ap_command_result receipt;
    bool original_collected;
    struct ap_stream_copy_owned stream_copy;
    struct ap_stream_tx_owned stream_tx;
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
static struct ap_pending_command *start(struct ap_session *s,u64 ticket,u64 call) {
    struct ap_pending_command *p=&s->pending[ap_command_slot(ticket)];
    assert(p->state==AP_SLOT_FREE);
    p->state=AP_SLOT_ACTIVE;p->submitted=(struct ap_task_command){.command=ticket,
        .operation=AP_ORIGINAL_READ,.expected_object=call,.original_count=69632};
    return p;
}
static struct ap_stream_copy_record data(u64 ticket,u64 call,u64 sequence,u64 attempt,u64 offset,u32 length) {
    struct ap_stream_copy_record r={.provider=3,.command=ticket,.call=call,
        .task=13,.task_start=17,.sequence=sequence,.attempt=attempt,.offset=offset,
        .length=length,.kind=AP_STREAM_COPY_DATA};
    assert(length && length<=AP_STREAM_COPY_BYTES);
    memset(r.bytes,'Z',length);return r;
}
static void unit(struct ap_session *s,u64 ticket,u64 call,u64 requested,u64 order,s64 returned) {
    struct ap_stream_copy_owned *copy=&s->pending[ap_command_slot(ticket)].stream_copy;
    struct ap_stream_copy_unit native={.file=23,.order=order,.offset=copy->unit_cursor,
        .requested=requested,.copied=copy->unit_bytes,.returned=returned,
        .position=copy->unit_cursor,.transport=AP_STREAM_COPY_TCP,.disposition=AP_STREAM_COPY_CONSUME};
    struct ap_stream_copy_record record={.provider=3,.command=ticket,.call=call,.task=13,.task_start=17,
        .sequence=copy->count+1,.attempt=copy->completed_units+1,.offset=copy->unit_cursor,
        .length=sizeof(native),.kind=AP_STREAM_COPY_UNIT};
    memcpy(record.bytes,&native,sizeof(native));assert(stream_copy_record(s,&record,sizeof(record))==0);
}
static void commit(struct ap_session *s,u64 ticket,u64 call,s64 returned,u64 final) {
    struct ap_pending_command *p=&s->pending[ap_command_slot(ticket)];
    struct ap_stream_copy_owned *copy=&p->stream_copy;
    struct ap_stream_copy_summary summary={.version=AP_STREAM_COPY_VERSION,.initial_count=69632,
        .attempts=copy->last_attempt,.records=copy->count,.copied=copy->copied,
        .final_count=final,.protocol_returned=(u64)returned,.protocol_complete=1};
    struct ap_stream_copy_record r={.provider=3,.command=ticket,.call=call,.task=13,.task_start=17,
        .sequence=copy->count+1,.attempt=copy->last_attempt,.offset=(u64)returned,
        .length=sizeof(summary),.kind=AP_STREAM_COPY_COMMIT};
    memcpy(r.bytes,&summary,sizeof(summary));assert(stream_copy_record(s,&r,sizeof(r))==0);
    assert(ap_original_read_copy_ready(s,91,ticket,0)==1);
    p->original_collected=true;p->state=AP_SLOT_COLLECTED;
    p->receipt=(struct ap_command_result){.command=ticket,.task=13,.start_boottime=17,.returned=returned};
}
static void dispose(struct ap_session *s) {
    for(unsigned i=0;i<AP_COMMANDS;i++)free(s->pending[i].stream_copy.records);
}
/* Exercise the same physical adapter as BPF, substituting only checked
 * kernel reads. Even correct copied words plus a failing read must refuse. */
static u64 recvmsg_slots[6],recvmsg_words[6];
static unsigned recvmsg_reads,recvmsg_fail_read;
static long recvmsg_read_error;
static long recvmsg_frame_read(void *to,u32 bytes,const void *from) {
    unsigned n=recvmsg_reads++;
    if(n>=6 || bytes!=8 || (u64)from!=recvmsg_slots[n])return -14;
    memcpy(to,&recvmsg_words[n],8);
    return recvmsg_fail_read==n+1?recvmsg_read_error:0;
}
static void recvmsg_frame_setup(void) {
    const u64 stack=0xffff800000001000ULL;
    const u64 slots[]={stack,stack+16,stack+24,stack+192,stack+208,0xffff800000002000ULL};
    const u64 words[]={0xffffffff8206bdd8ULL,0,0,0x70002000,0xffffffff8206a0aeULL,stack+32};
    memcpy(recvmsg_slots,slots,sizeof(slots));memcpy(recvmsg_words,words,sizeof(words));
    recvmsg_reads=recvmsg_fail_read=0;recvmsg_read_error=0;
}
static void recvmsg_physical_frame_controls(void) {
    const u64 functions[]={0xffffffff82355dc0ULL,0xffffffff820524a0ULL,0xffffffff8215a110ULL};
    unsigned checks=0;
#define RECVMSG_CHECK(x) do {assert(x);checks++;} while(0)
    for(unsigned protocol=1;protocol<=3;protocol++) {
        recvmsg_frame_setup();
        RECVMSG_CHECK(ap_stream_recvmsg_scratch(protocol,functions[protocol-1],0x70002000,
            recvmsg_slots[0],(const void *)recvmsg_slots[5],recvmsg_frame_read)==recvmsg_words[5]);
        RECVMSG_CHECK(recvmsg_reads==6);
        const long errors[]={-14,-5,1};
        for(unsigned read=1;read<=6;read++)for(unsigned error=0;error<3;error++) {
            recvmsg_frame_setup();recvmsg_fail_read=read;recvmsg_read_error=errors[error];
            RECVMSG_CHECK(!ap_stream_recvmsg_scratch(protocol,functions[protocol-1],0x70002000,
                recvmsg_slots[0],(const void *)recvmsg_slots[5],recvmsg_frame_read));
            RECVMSG_CHECK(recvmsg_reads==read);
        }
        for(unsigned bad=0;bad<13;bad++) {
            recvmsg_frame_setup();u64 stack=recvmsg_slots[0],name=recvmsg_slots[5];
            switch(bad) {
            case 0:recvmsg_words[3]=0;break;case 1:recvmsg_words[3]++;break;
            case 2:recvmsg_words[4]=0;break;case 3:recvmsg_words[4]++;break;
            case 4:recvmsg_words[4]=0xffffffff81dd7c4dULL;break;
            case 5:recvmsg_slots[3]-=8;break;case 6:recvmsg_slots[4]-=8;break;
            case 7:stack=0;break;case 8:stack++;break;
            case 9:stack=~0ULL-39;break; /* scratch fits; saved frame does not */
            case 10:name=0;break;case 11:recvmsg_words[0]=0x1d2a;break;
            case 12:recvmsg_words[4]-=8;break;
            }
            RECVMSG_CHECK(!ap_stream_recvmsg_scratch(protocol,functions[protocol-1],0x70002000,
                stack,(const void *)name,recvmsg_frame_read));
            if(bad>=7 && bad<=10)RECVMSG_CHECK(!recvmsg_reads);
        }
        recvmsg_frame_setup();
        RECVMSG_CHECK(!ap_stream_recvmsg_scratch(protocol,functions[protocol-1],0x70002000,
            recvmsg_slots[0],(const void *)recvmsg_slots[5],NULL));
        RECVMSG_CHECK(!recvmsg_reads);
    }
    assert(checks==171);
    printf("recvmsg checked saved-frame adapter: %u controls, exact caller R15 and outer return, all 6 read failures preserve refusal\n",checks);
#undef RECVMSG_CHECK
}
static void recvmsg_frame_controls(void) {
    const u64 functions[]={0xffffffff82355dc0ULL,0xffffffff820524a0ULL,0xffffffff8215a110ULL};
    for(unsigned protocol=1;protocol<=3;protocol++) {
        const u64 header=0x70002000;
        struct ap_stream_recvmsg_frame frame={.stack=0xffff800000001000ULL,
            .returned=0xffffffff8206bdd8ULL,.header=header,.name=0xffff800000001020ULL,
            .outer_return=0xffffffff8206a0aeULL};
        assert(ap_stream_recvmsg_frame_matches(protocol,functions[protocol-1],header,&frame));
        for(unsigned bad=0;bad<14;bad++) {
            struct ap_stream_recvmsg_frame other=frame;u64 function=functions[protocol-1],p=protocol,h=header;
            switch(bad) {
            case 0:other.stack=0;break;case 1:other.stack++;break;
            case 2:other.stack=~0ULL-7;other.name=24;break;
            case 3:other.header++;break;case 4:h=0;break;
            case 5:other.name=0;break;case 6:other.name+=8;break;
            case 7:other.user_name=header;break;case 8:other.user_control=header;break;
            case 9:other.returned++;break;
            /* The wrapper's no-security alternate is not this syscall's site. */
            case 10:other.returned=0xffffffff8206be95ULL;break;
            case 11:function=0;break;case 12:p=0;break;case 13:p=4;break;
            }
            assert(!ap_stream_recvmsg_frame_matches(p,function,h,&other));
        }
        assert(!ap_stream_recvmsg_frame_matches(protocol,functions[protocol-1],header,NULL));
    }
    for(unsigned scratch=0;scratch<2;scratch++) {
        const u64 name=scratch?0xffff800000001020ULL:0;
        struct ap_stream_message_view message={.name=name};
        assert(ap_stream_message_plain(&message,name,0));
        for(unsigned bad=0;bad<7;bad++) {
            struct ap_stream_message_view other=message;
            switch(bad) {
            case 0:other.name^=8;break;case 1:other.name_length=1;break;
            case 2:other.name_length=-1;break;case 3:other.control=0x70002000;break;
            case 4:other.length=1;break;case 5:other.flags=8;break;
            case 6:other.flags=0x40000000;break;
            }
            assert(!ap_stream_message_plain(&other,name,0));
        }
        assert(!ap_stream_message_plain(NULL,name,0));
    }
    struct ap_stream_message_view named={.name=0xffff800000001020ULL};
    for(s32 length=0;length<=128;length++) {
        named.name_length=length;
        assert(ap_stream_message_plain(&named,named.name,1));
        assert(ap_stream_message_plain(&named,named.name,0)==(length==0));
    }
    for(unsigned bad=0;bad<7;bad++) {
        struct ap_stream_message_view other={.name=named.name,.name_length=2};
        u64 expected=named.name;
        switch(bad) {
        case 0:other.name_length=-1;break;case 1:other.name_length=129;break;
        case 2:other.name+=8;break;case 3:other.control=0x70002000;break;
        case 4:other.length=1;break;case 5:other.flags=8;break;
        case 6:expected=other.name=0;break;
        }
        assert(!ap_stream_message_plain(&other,expected,1));
    }
    puts("recvmsg imported-frame controls: 3 exact protocol sites, 45 frame refusals, 2 plain headers, 16 name/control/MSG_CTRUNC refusals, 129 bounded internal Unix names and 7 return refusals");
}
static void copy_link_controls(void) {
    for(unsigned which=0;which<4;which++) {
        struct bpf_prog_info p={.type=BPF_PROG_TYPE_KPROBE,.id=71};
        struct bpf_link_info l={.type=BPF_LINK_TYPE_PERF_EVENT,.id=81,.prog_id=71};
        l.perf_event.type=BPF_PERF_EVENT_KPROBE;
        l.perf_event.kprobe.name_len=sizeof(AP_STREAM_COPY_SYMBOL);
        const u64 offsets[]={AP_STREAM_COPY_ENTRY_OFFSET,AP_STREAM_COPY_EXIT_OFFSET,
            AP_STREAM_COPY_FRAG_ENTRY_OFFSET,AP_STREAM_COPY_FRAG_EXIT_OFFSET};
        l.perf_event.kprobe.offset=offsets[which];l.perf_event.kprobe.cookie=5+which;
        assert(ap_stream_copy_link_matches(which,&p,sizeof(p),&l,sizeof(l),AP_STREAM_COPY_SYMBOL));
        for(unsigned bad=0;bad<12;bad++) {
            struct bpf_prog_info q=p;struct bpf_link_info m=l;
            u32 ps=sizeof(p),ls=sizeof(l);const char *name=AP_STREAM_COPY_SYMBOL;
            if(bad==0)q.recursion_misses=1;if(bad==1)m.perf_event.kprobe.missed=1;
            if(bad==2)m.perf_event.type=BPF_PERF_EVENT_KRETPROBE;
            if(bad==3)m.perf_event.kprobe.offset=1;if(bad==4)m.perf_event.kprobe.cookie=0;
            if(bad==5)m.perf_event.kprobe.name_len--;if(bad==6)name="simple_copy_to_iter";
            if(bad==7)m.prog_id++;if(bad==8)q.type=BPF_PROG_TYPE_TRACING;
            if(bad==9)m.type=BPF_LINK_TYPE_TRACING;
            if(bad==10)ps=offsetof(struct bpf_prog_info,recursion_misses);
            if(bad==11)ls=offsetof(struct bpf_link_info,perf_event.kprobe.missed);
            assert(!ap_stream_copy_link_matches(which,&q,ps,&m,ls,name));
        }
        assert(!ap_stream_copy_link_matches(4,&p,sizeof(p),&l,sizeof(l),AP_STREAM_COPY_SYMBOL));
    }
}
static void copy_pair_controls(void) {
    assert(ap_stream_copy_pair_matches(2,6,0x1065,0x106a));
    assert(ap_stream_copy_pair_matches(3,8,0x126c,0x1271));
    for(u64 cookie=0;cookie<10;cookie++)for(u64 phase=0;phase<5;phase++) {
        bool valid=(cookie==6 && phase==2) || (cookie==8 && phase==3);
        assert(!!ap_stream_copy_pair_matches(phase,cookie,0x1065,0x106a)==valid);
    }
    /* Indirect branches and zero-length alternatives join the same successor
     * without its direct entry. Production applies sticky stream_copy_problem. */
    assert(!ap_stream_copy_pair_matches(1,6,0,0x106a));
    assert(!ap_stream_copy_pair_matches(1,8,0,0x1271));
    assert(!ap_stream_copy_pair_matches(2,6,0,5));
    assert(!ap_stream_copy_pair_matches(2,6,~0ULL-4,0));
    assert(!ap_stream_copy_pair_matches(2,6,0x1065,0x1069));
    assert(!ap_stream_copy_pair_matches(3,8,0x1065,0x1271));
    puts("copy direct call pairing: both branches, alternative successors and mismatches refuse");
}
static void unit_caller_controls(void) {
    const u64 function=0xffffffff81fb85d0ULL;
    const u64 callers[]={0xffffffff81fb7523ULL,0xffffffff8206e119ULL,0xffffffff8215ac9aULL};
    for(unsigned i=0;i<3;i++) {
        u64 transport=i==2?AP_STREAM_COPY_UNIX:AP_STREAM_COPY_TCP;
        assert(ap_stream_copy_unit_caller(transport,function,callers[i]));
        assert(!ap_stream_copy_unit_caller(transport,function,callers[i]-1));
        assert(!ap_stream_copy_unit_caller(transport,function,callers[i]+1));
        assert(!ap_stream_copy_unit_caller(transport,0,callers[i]));
        assert(!ap_stream_copy_unit_caller(transport,function,0));
        assert(!ap_stream_copy_unit_caller(i==2?AP_STREAM_COPY_TCP:AP_STREAM_COPY_UNIX,function,callers[i]));
        assert(!ap_stream_copy_unit_caller(3,function,callers[i]));
    }
    assert(!ap_stream_copy_unit_caller(AP_STREAM_COPY_TCP,0x10ad,0));
    assert(!ap_stream_copy_unit_caller(AP_STREAM_COPY_TCP,~0ULL,0xb5b48));
    assert(!ap_stream_copy_unit_caller(AP_STREAM_COPY_UNIX,~0ULL,0x1a26c9));
    puts("copy unit caller: inline/outlined locked TCP and Unix actor,21 exact-site and3 overflow controls");
}
static void protocol_return_controls(void) {
    assert(ap_stream_copy_protocol_return(0)==0);
    assert(ap_stream_copy_protocol_return(64)==64);
    assert(ap_stream_copy_protocol_return(0x7fffffffULL)==2147483647LL);
    assert(ap_stream_copy_protocol_return(0x80000000ULL)==(-2147483647LL-1));
    assert(ap_stream_copy_protocol_return(0xfffffff2ULL)==-14);
    assert(ap_stream_copy_protocol_return(0xfffffffffffffff2ULL)==-14);
    assert(ap_stream_copy_protocol_return(0xffffffffULL)==-1);
    assert(ap_stream_copy_protocol_return(0x1234567800000040ULL)==64);
    puts("copy protocol signed32 ABI: positive,negative,zero-extended and unspecified high-register controls=8");
}
/* A failed observation follows the current physical frontier and leaves
 * the iterator unconsumed. Later successes must exceed that same frontier. */
static void failed_frontier_controls(void) {
    struct ring_buffer ring={0};
    struct ap_session s={.incarnation=3,.ready=true,.stream_copy_ring=&ring};
    struct ap_pending_command *p=start(&s,7,11);
    struct ap_stream_copy_record r=data(7,11,1,1,0,3);
    assert(stream_copy_record(&s,&r,sizeof(r))==0);unit(&s,7,11,3,4,0);
    r=data(7,11,3,2,3,2);assert(stream_copy_record(&s,&r,sizeof(r))==0);
    unit(&s,7,11,3,6,-EFAULT);
    assert(p->stream_copy.unit_order==6 && p->stream_copy.unit_cursor==3 && p->stream_copy.completed_units==2);
    struct ap_stream_copy_unit failed;memcpy(&failed,p->stream_copy.records[3].bytes,sizeof(failed));
    assert(failed.order==6 && failed.returned==-EFAULT && failed.offset==3 && failed.copied==2);
    r=data(7,11,5,3,3,3);assert(stream_copy_record(&s,&r,sizeof(r))==0);
    unit(&s,7,11,3,7,0);commit(&s,7,11,6,69626);
    assert(p->stream_copy.unit_order==7 && p->stream_copy.unit_cursor==6 && p->stream_copy.summary.copied==8);
    assert(AP_STREAM_COPY_VERSION==4 && p->stream_copy.summary.version==4);dispose(&s);
    for(unsigned bad=0;bad<4;bad++) {
        s=(struct ap_session){.incarnation=3,.ready=true,.stream_copy_ring=&ring};p=start(&s,7,11);
        r=data(7,11,1,1,0,2);assert(stream_copy_record(&s,&r,sizeof(r))==0);unit(&s,7,11,2,4,0);
        r=data(7,11,3,2,2,2);assert(stream_copy_record(&s,&r,sizeof(r))==0);
        struct ap_stream_copy_unit u={.file=23,.order=4,.offset=2,.requested=3,.copied=2,
            .returned=-EFAULT,.transport=AP_STREAM_COPY_TCP,.disposition=AP_STREAM_COPY_CONSUME};
        if(bad==0)u.order=3;
        if(bad==1)u.requested=2;
        if(bad==2) {u.returned=0;u.requested=2;}
        if(bad==3)u.returned=-EINTR;
        r=(struct ap_stream_copy_record){.provider=3,.command=7,.call=11,.task=13,.task_start=17,
            .sequence=4,.attempt=2,.offset=2,.length=sizeof(u),.kind=AP_STREAM_COPY_UNIT};
        memcpy(r.bytes,&u,sizeof(u));
        assert(stream_copy_record(&s,&r,sizeof(r))==-EPROTO && s.stream_copy_error==EPROTO);
        assert(p->stream_copy.unit_order==4 && p->stream_copy.unit_cursor==2 && p->stream_copy.completed_units==1);
        dispose(&s);
    }
    s=(struct ap_session){.incarnation=3,.ready=true,.stream_copy_ring=&ring};start(&s,7,11);
    struct ap_stream_copy_summary old={.version=2,.initial_count=69632,.final_count=69632,.protocol_complete=1};
    r=(struct ap_stream_copy_record){.provider=3,.command=7,.call=11,.task=13,.task_start=17,
        .sequence=1,.length=sizeof(old),.kind=AP_STREAM_COPY_COMMIT};memcpy(r.bytes,&old,sizeof(old));
    assert(stream_copy_record(&s,&r,sizeof(r))==-EPROTO && s.stream_copy_error==EPROTO);dispose(&s);
    puts("copy version4 failed frontier: nonzero observation, unchanged cursor, later consumption,4 stale/shape refusals and version2 refusal");
}
static void helper_disposition_controls(void) {
    struct ring_buffer ring={0};
    for(unsigned peek=0;peek<2;peek++) {
        struct ap_session s={.incarnation=3,.ready=true,.stream_copy_ring=&ring};
        struct ap_pending_command *p=start(&s,7,11);
        p->submitted.operation=peek?AP_ORIGINAL_RECVMSG_CALL:AP_ORIGINAL_RECVFROM_CALL;
        p->submitted.expected_option=peek?0x42:0x40;
        for(unsigned n=0;n<2;n++) {
            struct ap_stream_copy_record r=data(7,11,2*n+1,n+1,3*n,3);
            assert(stream_copy_record(&s,&r,sizeof(r))==0);
            struct ap_stream_copy_unit u={.file=23,.order=peek?4:4+n,.offset=3*n,
                .requested=3,.copied=3,.transport=1,.disposition=peek?AP_STREAM_COPY_OBSERVE:AP_STREAM_COPY_CONSUME};
            r=(struct ap_stream_copy_record){.provider=3,.command=7,.call=11,.task=13,.task_start=17,
                .sequence=2*n+2,.attempt=n+1,.offset=3*n,.length=sizeof(u),.kind=AP_STREAM_COPY_UNIT};
            memcpy(r.bytes,&u,sizeof(u));assert(stream_copy_record(&s,&r,sizeof(r))==0);
        }
        assert(p->stream_copy.unit_cursor==6 && p->stream_copy.unit_order==(peek?4:5));
        commit(&s,7,11,6,69626);assert(p->stream_copy.summary.copied==6);dispose(&s);
    }
    for(unsigned bad=0;bad<6;bad++) {
        struct ap_session s={.incarnation=3,.ready=true,.stream_copy_ring=&ring};
        struct ap_pending_command *p=start(&s,7,11);p->submitted.operation=AP_ORIGINAL_RECVMSG_CALL;
        p->submitted.expected_option=0x42;
        struct ap_stream_copy_record r=data(7,11,1,1,0,3);
        assert(stream_copy_record(&s,&r,sizeof(r))==0);
        struct ap_stream_copy_unit u={.file=23,.order=0,.requested=3,.copied=3,.transport=1,.disposition=AP_STREAM_COPY_OBSERVE};
        r=(struct ap_stream_copy_record){.provider=3,.command=7,.call=11,.task=13,.task_start=17,
            .sequence=2,.attempt=1,.length=sizeof(u),.kind=AP_STREAM_COPY_UNIT};
        if(bad==0)u.disposition=0;if(bad==1)u.disposition=3;if(bad==2)u.disposition=AP_STREAM_COPY_CONSUME;
        if(bad==3)p->submitted.expected_option=0x40;
        if(bad==4)r.length=64;
        if(bad==5)p->stream_copy.unit_order=1;
        memcpy(r.bytes,&u,sizeof(u));
        assert(stream_copy_record(&s,&r,sizeof(r))==-EPROTO && s.stream_copy_error==EPROTO);
        assert(!p->stream_copy.completed_units && !p->stream_copy.unit_visible && p->stream_copy.unit_cursor==0);
        dispose(&s);
    }
    for(unsigned old=0;old<2;old++) {
        struct ap_session s={.incarnation=3,.ready=true,.stream_copy_ring=&ring};
        struct ap_pending_command *p=start(&s,7,11);p->submitted.operation=AP_ORIGINAL_RECVMSG_CALL;
        p->submitted.expected_option=0x42;
        struct ap_stream_copy_summary m={0};
        if(old)m=(struct ap_stream_copy_summary){.version=3,.initial_count=69632,.final_count=69632,.protocol_complete=1};
        struct ap_stream_copy_record r={.provider=3,.command=7,.call=11,.task=13,.task_start=17,
            .sequence=1,.length=sizeof(m),.kind=AP_STREAM_COPY_COMMIT};memcpy(r.bytes,&m,sizeof(m));
        assert(stream_copy_record(&s,&r,sizeof(r))==-EPROTO && s.stream_copy_error==EPROTO);
        assert(!p->stream_copy.committed);dispose(&s);
    }
    puts("copy version4 helper dispositions:2 positive roles,6 unchanged-state refusals,absent/version3 refusal");
}
int main(void) {
    recvmsg_frame_controls();
    recvmsg_physical_frame_controls();
    helper_disposition_controls();
    failed_frontier_controls();
    protocol_return_controls();
    unit_caller_controls();
    copy_pair_controls();
    copy_link_controls();
    assert(ap_stream_copy_source_roles(0));
    assert(ap_stream_copy_source_roles(1));
    assert(ap_stream_copy_linear_take(2,3,10)==3);
    assert(ap_stream_copy_linear_take(8,5,10)==2);
    assert(ap_stream_copy_linear_take(10,5,10)==0);
    assert(ap_stream_copy_mixed_head_owned(2,3,15,5,1,0));
    assert(!ap_stream_copy_mixed_head_owned(2,3,15,5,2,0));
    assert(!ap_stream_copy_mixed_head_owned(2,3,15,5,1,1));
    assert(ap_stream_copy_tcp_storage_flags(0));
    /* ZEROCOPY_ENABLE, SHARED_FRAG, PURE_ZEROCOPY, DONT_ORPHAN,
     * MANAGED_FRAG_REFS and unknown flag bits are all refused. Unix splice
     * does not use this flags-only subpredicate; nonlinear Unix is excluded. */
    for(unsigned bit=0;bit<32;bit++)assert(!ap_stream_copy_tcp_storage_flags(1U<<bit));
    assert(!ap_stream_copy_tcp_storage_flags(0x1f));
    struct ring_buffer ring={0};
    struct ap_session s={.incarnation=3,.ready=true,.stream_copy_ring=&ring};
    struct ap_pending_command *p=start(&s,7,11);
    assert(ap_original_read_copy_ready(&s,91,7,0)==0);
    /* A different Call can interleave ring submissions; each command retains
     * its own exact sequence and original payload until delivery is complete. */
    start(&s,8,19);
    u64 sequence=0;
    for(u64 at=0;at<65534;at+=AP_STREAM_COPY_BYTES) {
        u32 count=65534-at;if(count>AP_STREAM_COPY_BYTES)count=AP_STREAM_COPY_BYTES;
        struct ap_stream_copy_record r=data(7,11,++sequence,1,at,count);
        assert(stream_copy_record(&s,&r,sizeof(r))==0);
        if(at==0) {r=data(8,19,1,1,0,3);assert(stream_copy_record(&s,&r,sizeof(r))==0);}
    }
    unit(&s,7,11,65534,1,0);
    unit(&s,8,19,16,0,-EFAULT);
    commit(&s,7,11,65534,4098);
    commit(&s,8,19,-EFAULT,69632);
    struct ap_stream_copy_manifest manifest;
    assert(ap_original_read_copy_manifest(&s,7,&manifest)==0);
    assert(manifest.present==1 && manifest.returned==65534 && manifest.summary.copied==65534);
    assert(manifest.summary.records==129 && manifest.summary.final_count==4098);
    struct ap_stream_copy_record r;
    assert(ap_original_read_copy_record(&s,7,1,&r)==-1 && errno==EINVAL);
    for(u64 i=0;i<128;i++) {
        assert(ap_original_read_copy_record(&s,7,i,&r)==0);
        assert(r.sequence==i+1 && r.offset==i*512 && r.call==11);
        for(u32 j=0;j<r.length;j++)assert(r.bytes[j]=='Z');
        assert(ap_original_read_copy_record(&s,7,i,&r)==0);
    }
    assert(p->stream_copy.delivered==128);
    assert(ap_original_read_copy_record(&s,7,128,&r)==0 && r.kind==AP_STREAM_COPY_UNIT);
    assert(p->stream_copy.delivered==129);
    assert(ap_original_read_copy_record(&s,7,129,&r)==-1 && errno==EINVAL);
    assert(ap_original_read_copy_manifest(&s,8,&manifest)==0);
    assert(manifest.returned==-EFAULT && manifest.summary.copied==3 && manifest.summary.final_count==69632);
    assert(ap_original_read_copy_record(&s,8,0,&r)==0 && r.call==19 && r.length==3);
    assert(ap_original_read_copy_record(&s,8,1,&r)==0 && r.kind==AP_STREAM_COPY_UNIT);
    struct ap_stream_copy_unit failed;memcpy(&failed,r.bytes,sizeof(failed));
    assert(failed.returned==-EFAULT && failed.copied==3 && failed.order==0);
    dispose(&s);
    for(unsigned bad=0;bad<9;bad++) {
        s=(struct ap_session){.incarnation=3,.ready=true,.stream_copy_ring=&ring};start(&s,7,11);
        r=data(7,11,1,1,0,4);
        switch(bad) {
        case 0:r.provider++;break;case 1:r.call++;break;case 2:r.command=0;break;
        case 3:r.sequence++;break;case 4:r.length=513;break;case 5:r.bytes[4]=1;break;
        case 6:r.kind=0;break;case 7:r.offset=AP_READ_MAX_COUNT;break;
        case 8:r.task_start=0;break;
        }
        assert(stream_copy_record(&s,&r,sizeof(r))==-EPROTO && s.stream_copy_error==EPROTO);
        assert(ap_drain_original_copy(&s)==-1 && errno==EPROTO);
        assert(s.pending[7].stream_copy.count==0);dispose(&s);
    }
    s=(struct ap_session){.incarnation=3,.ready=true,.stream_copy_ring=&ring};
    p=start(&s,7,11);
    assert(ap_original_read_copy_ready(&s,91,7,0)==0);
    assert(ap_original_read_copy_manifest(&s,7,&manifest)==-1 && errno==EINVAL);
    r=(struct ap_stream_copy_record){.provider=3,.command=7,.call=11,.task=13,.task_start=17,
        .sequence=1,.offset=7,.length=sizeof(struct ap_stream_copy_summary),.kind=AP_STREAM_COPY_COMMIT};
    assert(stream_copy_record(&s,&r,sizeof(r))==0);
    assert(ap_original_read_copy_ready(&s,91,7,0)==1);
    p->original_collected=true;p->state=AP_SLOT_COLLECTED;
    p->receipt=(struct ap_command_result){.command=7,.task=13,.start_boottime=17,.returned=7};
    assert(ap_original_read_copy_manifest(&s,7,&manifest)==0 && manifest.present==0 && manifest.returned==7);
    assert(p->stream_copy.committed && p->stream_copy.count==0);dispose(&s);
    for(unsigned bad=0;bad<7;bad++) {
        s=(struct ap_session){.incarnation=3,.ready=true,.stream_copy_ring=&ring};p=start(&s,7,11);
        r=(struct ap_stream_copy_record){.provider=3,.command=7,.call=11,.task=13,.task_start=17,
            .sequence=1,.offset=7,.length=sizeof(struct ap_stream_copy_summary),.kind=AP_STREAM_COPY_COMMIT};
        switch(bad) {
        case 0:r.provider++;break;case 1:r.call++;break;case 2:r.command++;break;
        case 3:r.sequence++;break;case 4:r.attempt=1;break;
        case 5:r.bytes[sizeof(struct ap_stream_copy_summary)]=1;break;
        case 6:assert(stream_copy_record(&s,&r,sizeof(r))==0);break;
        }
        assert(stream_copy_record(&s,&r,sizeof(r))==-EPROTO && s.stream_copy_error==EPROTO);
        assert(p->state==AP_SLOT_ACTIVE && p->stream_copy.count==0);
        assert(p->stream_copy.committed==(bad==6));dispose(&s);
    }
    s=(struct ap_session){.incarnation=3,.ready=true,.stream_copy_ring=&ring};
    p=start(&s,7,11);ring.ring=(struct ring){.producer=64};terminal=false;
    assert(ap_original_read_copy_ready(&s,91,7,1)==-1 && errno==EAGAIN);
    assert(!p->stream_copy.terminal_cut_set && p->state==AP_SLOT_ACTIVE);
    terminal=true;
    assert(ap_original_read_copy_ready(&s,91,7,1)==0);
    assert(p->stream_copy.terminal_cut_set && p->stream_copy.terminal_cut==64);
    ring.ring.producer=128;ring.ring.consumer=64;
    assert(ap_original_read_copy_ready(&s,91,7,1)==1);
    assert(p->stream_copy.terminal_cut==64 && p->state==AP_SLOT_ACTIVE);
    assert(!p->stream_copy.committed);dispose(&s);
    /* A task can die during the next unit after earlier complete units.
     * Hold the command until the finite ring cut, then retain the unmatched
     * DATA tail as diagnostic evidence without inventing a UNIT or EXIT. */
    s=(struct ap_session){.incarnation=3,.ready=true,.stream_copy_ring=&ring};
    p=start(&s,7,11);terminal=false;ring.ring=(struct ring){.producer=96};
    r=data(7,11,1,1,0,3);assert(stream_copy_record(&s,&r,sizeof(r))==0);
    struct ap_stream_copy_progress death;
    assert(ap_original_read_copy_progress(&s,91,7,&death)==0 && !death.records && !death.exited && !death.terminal);
    terminal=true;
    assert(ap_original_read_copy_progress(&s,91,7,&death)==0 && !death.records && !death.exited && !death.terminal);
    assert(p->stream_copy.terminal_cut==96 && !p->stream_copy.terminal_drained);
    ring.ring.consumer=96;ring.ring.producer=192;
    assert(ap_original_read_copy_progress(&s,91,7,&death)==0 && death.records==1 && !death.exited && death.terminal==1);
    assert(!death.protocol && !p->stream_copy.completed_units && p->state==AP_SLOT_ACTIVE);
    assert(ap_original_read_copy_record(&s,7,0,&r)==0 && r.kind==AP_STREAM_COPY_DATA && r.length==3);
    assert(p->stream_copy.delivered==1 && p->stream_copy.terminal_cut==96);dispose(&s);
    /* A's first unit must be visible while its syscall remains entered. B
     * can consume the next unit and return before A's later unit arrives.
     * No actual-EXIT marker is inferred from either empty progress snapshot. */
    s=(struct ap_session){.incarnation=3,.ready=true,.stream_copy_ring=&ring};
    terminal=false;ring.ring=(struct ring){0};
    p=start(&s,7,11);start(&s,8,19);
    r=data(7,11,1,1,0,2);assert(stream_copy_record(&s,&r,sizeof(r))==0);
    struct ap_stream_copy_progress progress;
    assert(ap_original_read_copy_progress(&s,91,7,&progress)==0 && !progress.records && !progress.exited);
    assert(ap_original_read_copy_record(&s,7,0,&r)==-1 && errno==EINVAL);
    unit(&s,7,11,2,1,0);
    assert(ap_original_read_copy_progress(&s,91,7,&progress)==0 && progress.records==2 && !progress.exited);
    assert(ap_original_read_copy_record(&s,7,0,&r)==0 && r.kind==AP_STREAM_COPY_DATA);
    assert(ap_original_read_copy_record(&s,7,1,&r)==0 && r.kind==AP_STREAM_COPY_UNIT);
    assert(p->state==AP_SLOT_ACTIVE && !p->original_collected && !p->stream_copy.committed);
    r=data(8,19,1,1,0,3);assert(stream_copy_record(&s,&r,sizeof(r))==0);
    unit(&s,8,19,3,2,0);commit(&s,8,19,3,69629);
    r=data(7,11,3,2,2,3);assert(stream_copy_record(&s,&r,sizeof(r))==0);
    unit(&s,7,11,3,3,0);
    assert(ap_original_read_copy_progress(&s,91,7,&progress)==0 && progress.records==4 && !progress.exited);
    assert(ap_original_read_copy_record(&s,7,2,&r)==0 && r.offset==2);
    assert(ap_original_read_copy_record(&s,7,3,&r)==0 && r.kind==AP_STREAM_COPY_UNIT);
    struct ap_stream_copy_unit last;memcpy(&last,r.bytes,sizeof(last));
    assert(last.file==23 && last.order==3 && p->stream_copy.unit_cursor==5);
    commit(&s,7,11,5,69627);
    assert(ap_original_read_copy_progress(&s,91,7,&progress)==0 && progress.records==4 && progress.exited==1);
    assert(p->stream_copy.delivered==4);dispose(&s);
    for(unsigned bad=0;bad<8;bad++) {
        s=(struct ap_session){.incarnation=3,.ready=true,.stream_copy_ring=&ring};p=start(&s,7,11);
        r=data(7,11,1,1,0,3);assert(stream_copy_record(&s,&r,sizeof(r))==0);
        struct ap_stream_copy_unit native={.file=23,.order=1,.requested=3,.copied=3,.transport=AP_STREAM_COPY_TCP,.disposition=AP_STREAM_COPY_CONSUME};
        r=(struct ap_stream_copy_record){.provider=3,.command=7,.call=11,.task=13,.task_start=17,
            .sequence=2,.attempt=1,.length=sizeof(native),.kind=AP_STREAM_COPY_UNIT};
        switch(bad) {
        case 0:native.file=0;break;case 1:native.order=0;break;case 2:native.offset=1;break;
        case 3:native.requested=4;break;case 4:native.copied=2;break;case 5:native.returned=-EFAULT;break;
        case 6:native.transport=3;break;case 7:native.position=1ULL<<32;break;
        }
        memcpy(r.bytes,&native,sizeof(native));
        assert(stream_copy_record(&s,&r,sizeof(r))==-EPROTO && s.stream_copy_error==EPROTO);
        assert(!p->stream_copy.unit_visible && !p->stream_copy.completed_units && !p->stream_copy.committed);
        dispose(&s);
    }
    puts("Read copy consumer: 65,534-byte prefix, interleaved Calls, fault observations, ordered delivery, delayed/absent commit, terminal frontier, pre-EXIT interleaved unit delivery, 9 data/8 unit/7 commit refusals");
    return 0;
}
