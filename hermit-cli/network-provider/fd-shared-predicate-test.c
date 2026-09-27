/* SPDX-License-Identifier: MIT */
/* Exact private operand forwarding and current-actor journal sampling. No BPF,
 * map, tracefs or native provider is used. All historical controls remain. */
#include <assert.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include "fd-effects.h"
#include "connect-copy.h"
static unsigned checks;
#define CHECK(x) do {assert(x);checks++;} while(0)
_Static_assert(sizeof(struct ap_selection_physical)==32,"four exact u64 operands");
_Static_assert(offsetof(struct ap_selection_physical,table)==24,"last physical operand");
_Static_assert(sizeof(struct ap_allocator_entry_snapshot)==40,"five allocator words");
_Static_assert(offsetof(struct ap_allocator_entry_snapshot,arg3)==32,"last allocator operand");
_Static_assert(sizeof(struct ap_recv_operand_snapshot)==56,"seven receive words");
_Static_assert(offsetof(struct ap_recv_operand_snapshot,source_length)==48,"last receive operand");
_Static_assert(sizeof(struct ap_epoll_operand_snapshot)==48,"six epoll words");
_Static_assert(offsetof(struct ap_epoll_operand_snapshot,pointer)==40,"last epoll operand");
static void physical_context(void) {
    struct ap_task_command c={.provider=3,.command=7,.operation=AP_ORIGINAL_CONNECT,
        .expected_object=11,.generation_before=0x4000,.generation_after=17,
        .expected_level=4,.expected_option=24};
    struct ap_selection_physical p={19,23,0x8000,29};
    struct ap_fd_call call={.command=7,.operation=AP_ORIGINAL_CONNECT,
        .function_ip=0x1000,.raw_table=0x8000,.original.selection={.command=7,.call=11,
        .owner_mm=17,.provider=3,.task=19,.task_start=23,.table=29,
        .user_address=0x4000,.requested_fd=4,.address_length=24}};
    const struct ap_fd_call before=call;const struct ap_selection_physical saved=p;
    CHECK(ap_fdget_context_shared(&call,&c,NULL,&p));
    CHECK(ap_fdget_context(&call,&c,NULL,p.task,p.start,p.raw_table,p.table));
    CHECK(!ap_fdget_context_shared(&call,&c,NULL,NULL));
    for(unsigned n=0;n<4;n++) {
        struct ap_selection_physical bad=p;
        switch(n) {case 0:bad.task++;break;case 1:bad.start++;break;
            case 2:bad.raw_table++;break;case 3:bad.table++;break;}
        CHECK(!ap_fdget_context_shared(&call,&c,NULL,&bad));
    }
    CHECK(!memcmp(&call,&before,sizeof(call)));CHECK(!memcmp(&p,&saved,sizeof(p)));
    c.operation=call.operation=AP_ACCEPT_EFFECT;
    struct ap_fd_accept a={.command=7,.task=19,.task_start=23,.table=29,
        .phases=AP_FD_ENTERED,.requested_fd=4,.flags=24};
    CHECK(ap_fdget_context_shared(&call,&c,&a,&p));
    CHECK(!ap_fdget_context_shared(&call,&c,NULL,&p));
    a.task_start++;CHECK(!ap_fdget_context_shared(&call,&c,&a,&p));
}
static void allocator_operands(void) {
    const u64 nrs[]={41,257,213,291};
    for(unsigned n=0;n<4;n++) {
        struct ap_task_command c={.operation=n==0?AP_ORIGINAL_SOCKET_CALL:
            n==1?AP_ORIGINAL_OPENAT_CALL:AP_ORIGINAL_EPOLL_CALL,
            .expected_level=n==0?2:n==1?-100:8,
            .generation_before=n==0?1:n==1?0x800000004000ULL:nrs[n],
            .expected_option=n==0?6:n==1?0x800:0,.original_count=n==1?0x123400000180ULL:0};
        struct ap_allocator_entry_snapshot p={nrs[n],0x123400000000ULL|(u32)c.expected_level,
            c.generation_before,(u64)(u32)c.expected_option,c.original_count};
        if(n>=2)p.arg1=p.arg2=p.arg3=~0ULL; /* unused kernel registers stay legal */
        const struct ap_allocator_entry_snapshot saved=p;
        CHECK(ap_original_allocator_operands_shared(&c,&p));
        CHECK(ap_original_allocator_operands(&c,p.nr,p.arg0,p.arg1,p.arg2,p.arg3));
        CHECK(!ap_original_allocator_operands_shared(&c,NULL));
        CHECK(!ap_original_allocator_operands_shared(NULL,&p));
        struct ap_allocator_entry_snapshot bad=p;bad.nr++;CHECK(!ap_original_allocator_operands_shared(&c,&bad));
        bad=p;bad.arg0++;CHECK(!ap_original_allocator_operands_shared(&c,&bad));
        if(n<2) {bad=p;bad.arg1++;CHECK(!ap_original_allocator_operands_shared(&c,&bad));
            bad=p;bad.arg2++;CHECK(!ap_original_allocator_operands_shared(&c,&bad));}
        if(n==1) {bad=p;bad.arg3^=1ULL<<63;CHECK(!ap_original_allocator_operands_shared(&c,&bad));}
        CHECK(!memcmp(&p,&saved,sizeof(p)));
    }
}
static void receive_operands(void) {
    for(unsigned peek=0;peek<2;peek++) {
        struct ap_task_command c={.provider=3,.command=7,.expected_object=11,
            .operation=peek?AP_ORIGINAL_RECVMSG_CALL:AP_ORIGINAL_RECVFROM_CALL,
            .expected_level=5,.expected_option=peek?0x42:0x40,
            .generation_before=0x800000004000ULL,.original_count=peek?1024:8};
        struct ap_recv_operand_snapshot p={peek?47:45,0x123400000005ULL,c.generation_before,
            peek?0x42:c.original_count,0x40,0,0};
        CHECK(ap_original_recv_operands_shared(&c,&p));
        CHECK(ap_original_recv_operands(&c,p.nr,p.fd,p.address,p.count_or_flags,p.flags,p.source,p.source_length));
        CHECK(!ap_original_recv_operands_shared(&c,NULL));CHECK(!ap_original_recv_operands_shared(NULL,&p));
        for(unsigned n=0;n<(peek?4:7);n++) {
            struct ap_recv_operand_snapshot bad=p;
            switch(n) {case 0:bad.nr++;break;case 1:bad.fd++;break;case 2:bad.address^=1ULL<<63;break;
                case 3:bad.count_or_flags++;break;case 4:bad.flags++;break;
                case 5:bad.source=1;break;case 6:bad.source_length=1;break;}
            CHECK(!ap_original_recv_operands_shared(&c,&bad));
        }
        if(peek) {p.flags=p.source=p.source_length=~0ULL;CHECK(ap_original_recv_operands_shared(&c,&p));}
    }
}
static void epoll_operands(void) {
    struct ap_task_command c={.provider=3,.command=7,.expected_object=11,
        .operation=AP_ORIGINAL_EPOLL_CTL,.expected_level=5,.expected_option=1,
        .generation_before=0x800000004000ULL,.original_count=9};
    struct ap_epoll_operand_snapshot p={233,0x33,0x123400000005ULL,0x567800000001ULL,
        0x900000009ULL,c.generation_before};
    CHECK(ap_original_epoll_ctl_operands_shared(&c,&p));
    CHECK(ap_original_epoll_ctl_operands(&c,p.nr,p.cs,p.epfd,p.op,p.fd,p.pointer));
    CHECK(!ap_original_epoll_ctl_operands_shared(&c,NULL));CHECK(!ap_original_epoll_ctl_operands_shared(NULL,&p));
    for(unsigned n=0;n<6;n++) {
        struct ap_epoll_operand_snapshot bad=p;
        switch(n) {case 0:bad.nr++;break;case 1:bad.cs^=1ULL<<32;break;case 2:bad.epfd++;break;
            case 3:bad.op++;break;case 4:bad.fd++;break;case 5:bad.pointer^=1ULL<<63;break;}
        CHECK(!ap_original_epoll_ctl_operands_shared(&c,&bad));
    }
}

/* Exercise the real publisher with boundary substitutes. Actor sampling must
 * occur once per event and before any sequence allocation or row publication. */
#define INLINE static __attribute__((always_inline))
#define CORE(value) (value)
#define BPF_NOEXIST 1ULL
static struct ap_fd_status status;
static struct ap_fd_event row,expected;
static int fd_journal;
static unsigned task_reads,start_reads,published;
static unsigned explicit_actor;
static struct {u64 start_boottime;} current;
static u64 pid_tgid(void) {CHECK(!task_reads&&!start_reads);task_reads++;return expected.task;}
static typeof(current) *current_task(void) {CHECK(task_reads==1&&!start_reads);start_reads++;return &current;}
static struct ap_fd_status *fd_stats(void) {
    if(explicit_actor) {CHECK(!task_reads&&!start_reads);return &status;}
    CHECK(task_reads==1&&start_reads==1);return &status;
}
static void fd_problem(u64 problem) {(void)problem;assert(0);}
static long update(void *map,const void *key,const void *value,u64 flags) {
    CHECK(map==&fd_journal && flags==BPF_NOEXIST);CHECK(*(const u64 *)key==expected.sequence);
    const struct ap_fd_event pending={.sequence=expected.sequence,.complete=2};
    CHECK(!memcmp(value,&pending,sizeof(pending)));row=pending;return 0;
}
static void *lookup(void *map,const void *key) {
    CHECK(map==&fd_journal && *(const u64 *)key==expected.sequence);return &row;
}
static u64 publish(u64 *at,u64 before,u64 after) {
    if(explicit_actor) {CHECK(!task_reads&&!start_reads&&!published);}
    else {CHECK(task_reads==1&&start_reads==1&&!published);}
    published++;
    CHECK(at==&row.complete&&before==2&&after==1);CHECK(!memcmp(&row,&expected,sizeof(row)));
    row.complete=after;return before;
}
#define __sync_val_compare_and_swap(p,a,b) publish((p),(a),(b))
#include "fd-journal.bpf.h"
#undef __sync_val_compare_and_swap
static void current_journal_actor(void) {
    for(unsigned i=0;i<3;i++) {
        expected=(struct ap_fd_event){.sequence=i+1,.complete=2,.kind=AP_FD_REMOVE,
            .task=0x8000000000000001ULL+i,.task_start=~0ULL-i,.table=7,.fd=-3,
            .file=11,.previous_file=13,.dependency=17,.accept_command=19,.returned=-5};
        current.start_boottime=expected.task_start;task_reads=start_reads=published=0;
        CHECK(fd_event(expected.kind,expected.table,expected.fd,expected.file,
            expected.previous_file,expected.dependency,expected.accept_command,expected.returned)==i+1);
        CHECK(task_reads==1&&start_reads==1&&published==1);expected.complete=1;
        CHECK(!memcmp(&row,&expected,sizeof(row)));
    }
}
/* Supplied actor/profile is a different existing public wrapper. It must
 * not sample the current actor at all; the three current-actor cases above
 * retain their original positive read-count and publication assertions. */
static void explicit_journal_actor(void) {
    explicit_actor=1;task_reads=start_reads=published=0;
    expected=(struct ap_fd_event){.sequence=4,.complete=2,.kind=AP_FD_ENROLL_BEGIN,
        .task=0xa000000000000007ULL,.task_start=0xb000000000000009ULL,.table=31,.fd=-1,
        .file=37,.previous_file=41,.dependency=43,.accept_command=47,.returned=-9};
    CHECK(current.start_boottime!=expected.task_start);
    CHECK(fd_event_for(expected.task,expected.task_start,expected.kind,expected.table,
        expected.fd,expected.file,expected.previous_file,expected.dependency,
        expected.accept_command,expected.returned)==4);
    CHECK(!task_reads&&!start_reads&&published==1);
    expected.complete=1;CHECK(!memcmp(&row,&expected,sizeof(row)));
}

_Static_assert(sizeof(struct ap_invocation_key)==32,"full four-word key");
_Static_assert(sizeof(struct ap_fd_call)==424,"unchanged private Call object");
_Static_assert(sizeof(struct ap_copy_frame_snapshot)==48,"six full-width frame words");
_Static_assert(offsetof(struct ap_copy_frame_snapshot,address)==40,"last frame word");
_Static_assert(offsetof(struct ap_command_result,task)==16,"READY payload start");
_Static_assert(offsetof(struct ap_command_result,phase)==120,"READY payload end");
static void ready_payload_controls(void) {
    struct ap_command_result clean={.command=~0ULL,.operation=~0ULL,
        .phase=~0ULL,.original_count=~0ULL};
    CHECK(ap_command_ready_payload_empty(&clean));
    CHECK(!ap_command_ready_payload_empty(NULL));
    /* The twenty original fields cover all104 bytes from task through
     * reserved. Every bit is independently nonzero; reservation operands are
     * deliberately nonzero and remain the separate caller's responsibility. */
    for(unsigned byte=16;byte<120;byte++)for(unsigned bit=0;bit<8;bit++) {
        struct ap_command_result bad=clean;((u8 *)&bad)[byte]=(u8)(1U<<bit);
        const struct ap_command_result before=bad;
        CHECK(!ap_command_ready_payload_empty(&bad));
        CHECK(!memcmp(&bad,&before,sizeof(bad)));
    }
}
static void private_call_zero_controls(void) {
    struct {u64 before;struct ap_fd_call call;u64 after;} guarded;
    _Static_assert(offsetof(typeof(guarded),call)==8,"leading guard adjacent");
    _Static_assert(offsetof(typeof(guarded),after)==432,"trailing guard adjacent");
    memset(&guarded,0xa5,sizeof(guarded));ap_fd_call_clear(&guarded.call);
    CHECK(guarded.before==0xa5a5a5a5a5a5a5a5ULL && guarded.after==guarded.before);
    for(unsigned n=0;n<sizeof(guarded.call);n++)CHECK(((u8 *)&guarded.call)[n]==0);
    /* Representative supplied fields are independent of complete zeroing. */
    guarded.call.command=7;guarded.call.operation=AP_ORIGINAL_CONNECT;
    guarded.call.raw_table=0x1000;guarded.call.function_ip=0x2000;
    const struct ap_fd_call expected={.command=7,.operation=AP_ORIGINAL_CONNECT,
        .raw_table=0x1000,.function_ip=0x2000};
    CHECK(!memcmp(&guarded.call,&expected,sizeof(expected)));
}
static void event_zero_controls(void) {
    struct ap_epoll_ctl_copy copy={0};struct ap_original_epoll_ctl actual={0};
    CHECK(ap_epoll_ctl_event_empty(&copy));CHECK(ap_original_epoll_event_empty(&actual));
    for(unsigned byte=0;byte<12;byte++)for(unsigned bit=0;bit<8;bit++) {
        copy.event[byte]=(u8)(1U<<bit);actual.event[byte]=(u8)(1U<<bit);
        const struct ap_epoll_ctl_copy before=copy;const struct ap_original_epoll_ctl saved=actual;
        CHECK(!ap_epoll_ctl_event_empty(&copy));CHECK(!ap_original_epoll_event_empty(&actual));
        CHECK(!memcmp(&copy,&before,sizeof(copy)));CHECK(!memcmp(&actual,&saved,sizeof(actual)));
        copy.event[byte]=0;actual.event[byte]=0;
    }
}
static void shared_copy_frame_controls(void) {
    struct ap_task_command c={.provider=3,.command=7,.operation=AP_ORIGINAL_CONNECT,
        .expected_object=11,.generation_before=0x4000,.generation_after=17,
        .expected_level=4,.expected_option=24};
    struct ap_fd_call call={.command=7,.function_ip=0x1000,.operation=AP_ORIGINAL_CONNECT,
        .selection={.entered=1,.returned=1,.word=0x8001},
        .original.selection={.command=7,.call=11,.owner_mm=17,.provider=3,.task=19,.task_start=23,
        .table=29,.file=31,.user_address=0x4000,.fdput_flags=1,.ready=1,.requested_fd=4,.address_length=24}};
    struct ap_copy_frame_snapshot p={19,23,0x6000,0x8001,24,0x4000};
    const struct ap_fd_call before=call;const struct ap_copy_frame_snapshot saved=p;
    CHECK(ap_original_copy_frame_shared(&call,&c,&p));
    CHECK(ap_original_copy_frame(&call,&c,p.task,p.start,p.stack,p.file_word,p.length,p.address));
    CHECK(!ap_original_copy_frame_shared(&call,&c,NULL));
    CHECK(!ap_original_copy_frame_shared(NULL,&c,&p));CHECK(!ap_original_copy_frame_shared(&call,NULL,&p));
    for(unsigned n=0;n<6;n++) {
        struct ap_copy_frame_snapshot bad=p;
        switch(n) {case 0:bad.task++;break;case 1:bad.start++;break;case 2:bad.stack=0;break;
            case 3:bad.file_word++;break;case 4:bad.length++;break;case 5:bad.address^=1ULL<<63;break;}
        CHECK(!ap_original_copy_frame_shared(&call,&c,&bad));
    }
    CHECK(!memcmp(&call,&before,sizeof(call)));CHECK(!memcmp(&p,&saved,sizeof(p)));
}


/* The real shared kernel-boundary helper is included with only its existing
 * external operations substituted. The supplied key and selected output are
 * checked before any synthetic map lookup/publication can succeed. */
static struct ap_invocation_key shared_actor;
static struct ap_fd_call shared_call;
static int shared_map;
static unsigned shared_actor_reads,shared_lookups,shared_file_reads,shared_cas;
static int shared_absent,shared_missing_file;
static struct ap_invocation_key shared_actor_read(void) {
    CHECK(!shared_actor_reads&&!shared_lookups);shared_actor_reads++;return shared_actor;
}
static void *shared_lookup(void *map,const struct ap_invocation_key *key) {
    CHECK(map==&shared_map && shared_actor_reads==1 && !shared_lookups);
    CHECK(!memcmp(key,&shared_actor,sizeof(*key)));shared_lookups++;
    return shared_absent?NULL:&shared_call;
}
struct file;
static u64 shared_file(struct file *file) {
    CHECK((u64)file==0x8000 && !shared_file_reads);shared_file_reads++;
    return shared_missing_file?0:37;
}
static u64 shared_selection_cas(u64 *at,u64 before,u64 after) {
    CHECK(at==&shared_call.original.selection.ready && !shared_cas && before==0 && after==1);
    CHECK(shared_call.original.selection.file==shared_call.selected_file);
    CHECK(shared_call.original.selection.fdput_flags==(shared_call.selection.word&1));
    shared_cas++;const u64 found=*at;if(found==before)*at=after;return found;
}
#define fd_actor shared_actor_read
#define fd_calls shared_map
#define lookup shared_lookup
#define fd_file shared_file
#define __sync_val_compare_and_swap(p,a,b) shared_selection_cas((p),(a),(b))
#include "fd-call-shared.bpf.h"
#undef fd_actor
#undef fd_calls
#undef lookup
#undef fd_file
#undef __sync_val_compare_and_swap
static void shared_kernel_boundaries(void) {
    _Static_assert(sizeof(struct ap_fd_event_seed)==56,"six words and two signed operands");
    _Static_assert(offsetof(struct ap_fd_event_seed,returned)==52,"no unused seed tail");
    for(unsigned absent=0;absent<2;absent++)for(unsigned n=0;n<8;n++) {
        shared_actor=(struct ap_invocation_key){.task=0x8000000000000000ULL+n,.start=~0ULL-n};
        shared_actor_reads=shared_lookups=0;shared_absent=absent;
        struct ap_invocation_key key;memset(&key,0xa5,sizeof(key));
        CHECK(fd_actor_call(&key)==(absent?NULL:&shared_call));
        CHECK(!memcmp(&key,&shared_actor,sizeof(key)) && shared_actor_reads==1 && shared_lookups==1);
    }
    for(unsigned accept=0;accept<2;accept++)for(unsigned empty=0;empty<2;empty++)
    for(unsigned missing=0;missing<2;missing++)for(unsigned ready=0;ready<2;ready++)
    for(unsigned flag=0;flag<2;flag++) {
        shared_call=(struct ap_fd_call){.selection.word=empty?0:0x8000+flag,
            .original.selection.ready=ready?9:0};
        struct ap_fd_accept a={0};struct ap_fd_call want=shared_call;
        shared_file_reads=shared_cas=0;shared_missing_file=missing;
        const int failed=!empty&&missing;
        want.selected_file=empty||missing?0:37;
        if(failed) {if(!accept)want.original.problem=AP_FD_MISSING;}
        else if(!accept) {
            want.original.selection.file=want.selected_file;
            want.original.selection.fdput_flags=want.selection.word&1;
            if(ready)want.original.problem=AP_FD_DUPLICATE;else want.original.selection.ready=1;
        }
        fd_selected_file(&shared_call,accept?&a:NULL,!accept);
        CHECK(!memcmp(&shared_call,&want,sizeof(want)));
        CHECK(a.problem==(u64)(accept&&failed?AP_FD_MISSING:0));
        CHECK(shared_file_reads==!empty && shared_cas==(unsigned)(!accept&&!failed));
    }
}
static void unpublished_selection_controls(void) {
    struct ap_task_command c={.operation=AP_ORIGINAL_READ,.provider=3,.command=7,
        .expected_object=11,.generation_after=13,.expected_level=5,.generation_before=0x800000004000ULL,.original_count=19};
    struct ap_selection_physical p={23,29,0x6000,31};
    struct ap_fd_call call={.operation=c.operation,.command=7,.file_entry_ip=0x1000,.raw_table=0x6000,
        .original.selection={.provider=3,.command=7,.call=11,.owner_mm=13,.task=23,.task_start=29,
            .table=31,.requested_fd=5,.user_address=c.generation_before,.original_count=19}};
    CHECK(ap_read_selection_context_shared(&call,&c,&p));
    CHECK(ap_unpublished_selection_context(&call,&c,&p));
    CHECK(!ap_unpublished_selection_context(NULL,&c,&p));
    CHECK(!ap_unpublished_selection_context(&call,NULL,&p));
    CHECK(!ap_unpublished_selection_context(&call,&c,NULL));
    for(unsigned n=0;n<21;n++) {
        struct ap_fd_call bad=call;
        switch(n) {
        case 0:bad.operation++;break;case 1:bad.command++;break;case 2:bad.file_entry_ip=0;break;
        case 3:bad.raw_table++;break;case 4:bad.original.problem=1;break;case 5:bad.original.complete=1;break;
        case 6:bad.original.selection.ready=1;break;case 7:bad.original.selection.provider++;break;
        case 8:bad.original.selection.command++;break;case 9:bad.original.selection.call++;break;
        case 10:bad.original.selection.owner_mm++;break;case 11:bad.original.selection.task++;break;
        case 12:bad.original.selection.task_start++;break;case 13:bad.original.selection.table++;break;
        case 14:bad.original.selection.requested_fd++;break;case 15:bad.original.selection.user_address++;break;
        case 16:bad.original.selection.file=1;break;case 17:bad.original.selection.fdput_flags=1;break;
        case 18:bad.original.selection.original_count++;break;case 19:bad.original.selection.address_length=1;break;
        case 20:bad.original.selection.call=0;break;
        }
        const struct ap_fd_call saved=bad;
        CHECK(!ap_read_selection_context_shared(&bad,&c,&p));CHECK(!memcmp(&bad,&saved,sizeof(bad)));
    }
    for(unsigned op=0;op<2;op++) {
        c.operation=call.operation=op?AP_AUXILIARY_FILE:AP_ORIGINAL_FILE;c.original_count=0;
        c.expected_option=AP_FILE_GET_FLAGS;c.generation_before=AP_FILE_SYSCALL_FCNTL;
        call.original.selection.user_address=c.generation_before;
        call.original.selection.address_length=3;call.original.selection.original_count=0;
        CHECK(ap_file_selection_context_shared(&call,&c,&p));
        call.original.selection.original_count=1;CHECK(!ap_file_selection_context_shared(&call,&c,&p));
        call.original.selection.original_count=0;
    }
}
static void seeded_journal_controls(void) {
    explicit_actor=0;
    for(unsigned n=0;n<8;n++) {
        expected=(struct ap_fd_event){.sequence=5+n,.complete=2,
            .task=0x8000000000000000ULL+n,.task_start=~0ULL-n,
            .kind=0x100000000ULL+n,.table=0x200000000ULL+n,.file=0x300000000ULL+n,
            .previous_file=0x400000000ULL+n,.dependency=0x500000000ULL+n,
            .accept_command=0x600000000ULL+n,.fd=-31-(s32)n,.returned=37+(s32)n};
        current.start_boottime=expected.task_start;task_reads=start_reads=published=0;
        const struct ap_fd_event_seed seed={.kind=expected.kind,.table=expected.table,
            .file=expected.file,.previous_file=expected.previous_file,.dependency=expected.dependency,
            .accept_command=expected.accept_command,.fd=expected.fd,.returned=expected.returned};
        const struct ap_fd_event_seed saved=seed;
        CHECK(fd_event_current(&seed)==expected.sequence);
        CHECK(!memcmp(&seed,&saved,sizeof(seed)) && task_reads==1 && start_reads==1 && published==1);
        expected.complete=1;CHECK(!memcmp(&row,&expected,sizeof(row)));
    }
}
static void native_fdget_context_controls(void) {
    struct ap_task_command c={.operation=AP_ORIGINAL_READ,.provider=3,.command=7,
        .expected_object=11,.generation_after=13,.expected_level=5,.generation_before=0x4000,.original_count=19};
    struct ap_command_result r={.command=7,.operation=AP_ORIGINAL_READ,.phase=AP_COMMAND_RUNNING,
        .task=23,.start_boottime=29,.identity.provider=3,.original_count=19};
    struct ap_fd_call call={.operation=c.operation,.command=7,.selected_file=37,.entry_stack=0x1000,
        .selection={.entered=1,.returned=1,.word=0x8001},
        .original.selection={.provider=3,.command=7,.call=11,.owner_mm=13,.task=23,.task_start=29,
            .table=31,.requested_fd=5,.user_address=0x4000,.original_count=19,.file=37,.fdput_flags=1,.ready=1}};
    CHECK(ap_original_fdget_native_context(&c,&r,&call,23,29));
    CHECK(ap_original_read_native_return(&c,&r,&call,23,29,AP_READ_SYSCALL,5,0x4000,19,9));
    for(unsigned n=0;n<8;n++) {
        struct ap_command_result bad=r;
        switch(n) {case 0:bad.operation++;break;case 1:bad.command++;break;case 2:bad.phase=AP_COMMAND_READY;break;
            case 3:bad.task++;break;case 4:bad.start_boottime++;break;case 5:bad.identity.provider++;break;
            case 6:bad.task=0;break;case 7:bad.start_boottime=0;break;}
        const struct ap_command_result saved=bad;
        CHECK(!ap_original_fdget_native_context(&c,&bad,&call,23,29));
        CHECK(!memcmp(&bad,&saved,sizeof(bad)));
    }
    for(unsigned n=0;n<12;n++) {
        struct ap_fd_call bad=call;
        switch(n) {case 0:bad.operation++;break;case 1:bad.command++;break;case 2:bad.selection.entered=0;break;
            case 3:bad.selection.returned=0;break;case 4:bad.selected_file++;break;case 5:bad.selection.word=1;break;
            case 6:bad.selection.word=0x8000;break;case 7:bad.original.complete=1;break;case 8:bad.original.problem=1;break;
            case 9:bad.original.selection.task++;break;case 10:bad.original.selection.task_start++;break;
            case 11:bad.original.selection.ready=0;break;}
        const struct ap_fd_call saved=bad;
        CHECK(!ap_original_fdget_native_context(&c,&r,&bad,23,29));CHECK(!memcmp(&bad,&saved,sizeof(bad)));
    }
    CHECK(!ap_original_fdget_native_context(NULL,&r,&call,23,29));
    CHECK(!ap_original_fdget_native_context(&c,NULL,&call,23,29));
    CHECK(!ap_original_fdget_native_context(&c,&r,NULL,23,29));
    call.entry_stack=0;CHECK(ap_original_fdget_native_context(&c,&r,&call,23,29));
    CHECK(!ap_original_read_native_return(&c,&r,&call,23,29,AP_READ_SYSCALL,5,0x4000,19,9));
    for(unsigned op=0;op<2;op++) {
        c.operation=r.operation=call.operation=op?AP_AUXILIARY_FILE:AP_ORIGINAL_FILE;
        c.original_count=r.original_count=call.original.selection.original_count=0;
        c.generation_before=call.original.selection.user_address=AP_FILE_SYSCALL_FCNTL;
        c.expected_option=call.original.selection.address_length=AP_FILE_GET_FLAGS;
        CHECK(ap_original_fdget_native_context(&c,&r,&call,23,29));
        CHECK(ap_original_file_native_return(&c,&r,&call,23,29,AP_FILE_SYSCALL_FCNTL,5,AP_FILE_GET_FLAGS,0x800));
        call.entry_stack=1;CHECK(!ap_original_file_native_return(&c,&r,&call,23,29,AP_FILE_SYSCALL_FCNTL,5,AP_FILE_GET_FLAGS,0x800));
        call.entry_stack=0;
    }
}

int main(void) {
    (void)ap_require_retirement_target;
    physical_context();allocator_operands();receive_operands();epoll_operands();current_journal_actor();explicit_journal_actor();
    CHECK(checks==126);
    ready_payload_controls();private_call_zero_controls();event_zero_controls();shared_copy_frame_controls();
    CHECK(checks==2618);
    shared_kernel_boundaries();unpublished_selection_controls();seeded_journal_controls();native_fdget_context_controls();
    printf("shared predicates: %u exact operand, refusal, immutable snapshot and fresh-actor assertions\n",checks);
    return 0;
}
