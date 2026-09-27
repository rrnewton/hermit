/* SPDX-License-Identifier: MIT */
#ifdef AP_FTRACE_MUTANT_ONLY
#include <assert.h>
#include <string.h>
#include "fd-effects.h"
#include "connect-copy.h"

static long mutant_copy(void *to,u32 size,const void *from) {
    memcpy(to,from,size);return 0;
}
static struct ap_task_command mutant_connect_owner(void) {
    return (struct ap_task_command){.provider=3,.command=7,.operation=AP_ORIGINAL_CONNECT,
        .expected_object=11,.generation_before=0x4000,.generation_after=17,
        .expected_level=4,.expected_option=24};
}
static struct ap_fd_call mutant_connect_call(void) {
    return (struct ap_fd_call){.command=7,.function_ip=0x1000,.raw_table=0x2000,
        .operation=AP_ORIGINAL_CONNECT,.selection={.entered=1,.returned=1,.word=0x8001},
        .original.selection={.command=7,.call=11,.owner_mm=17,.provider=3,
            .task=19,.task_start=23,.table=29,.file=31,.user_address=0x4000,
            .fdput_flags=1,.ready=1,.requested_fd=4,.address_length=24}};
}
static struct ap_task_command mutant_read_owner(void) {
    return (struct ap_task_command){.provider=3,.command=7,.operation=AP_ORIGINAL_READ,
        .expected_object=11,.generation_before=0x10000000004ULL,.generation_after=17,
        .expected_level=4,.original_count=0x10000000020ULL};
}
static struct ap_fd_call mutant_read_call(void) {
    struct ap_task_command owner=mutant_read_owner();
    return (struct ap_fd_call){.command=7,.file_entry_ip=0x1014,.raw_table=0x2000,
        .operation=AP_ORIGINAL_READ,.original.selection={.command=7,.call=11,
            .owner_mm=17,.provider=3,.task=19,.task_start=23,.table=29,
            .user_address=owner.generation_before,.requested_fd=4,
            .original_count=owner.original_count}};
}
int main(void) {
    (void)ap_require_retirement_target;
    assert(ap_shared_fdget_role(AP_ORIGINAL_CONNECT,0x1000,0x101c)==1);
    assert(ap_shared_fdget_role(AP_ACCEPT_EFFECT,0x1000,0x1021)==4);
    assert(ap_shared_fdget_role(AP_ORIGINAL_EPOLL_CTL,0x1000,0x1023)==10);
    assert(ap_shared_fdget_role(AP_ORIGINAL_EPOLL_CTL,0x1000,0x1037)==11);
    struct ap_task_command connect=mutant_connect_owner();
    struct ap_fd_call call=mutant_connect_call();
    assert(ap_original_copy_infer_success(
        &call,&connect,19,23,0x5ff8,0x6000,0x8001,24,0x4000));
    struct ap_task_command file={.provider=3,.command=7,.operation=AP_ORIGINAL_FILE,
        .expected_object=11,.generation_before=72,.generation_after=17,
        .expected_level=4,.expected_option=3};
    call=(struct ap_fd_call){.command=7,.file_entry_ip=0x1000,.raw_table=0x2000,
        .operation=AP_ORIGINAL_FILE,.original.selection={.command=7,.call=11,
            .owner_mm=17,.provider=3,.task=19,.task_start=23,.table=29,
            .user_address=72,.requested_fd=4,.address_length=3}};
    assert(ap_file_selection_enter(&call,&file,19,23,0x2000,29,12,0x6000));
    assert(ap_file_selection_session_post(&call,&file,19,23,0x2000,29,0x8001));
    assert(ap_file_fdget_caller_site(0xffffffff81faaca0ULL,0xffffffff81facb54ULL));
    struct ap_task_command read=mutant_read_owner();
    s64 nr=0;s32 fd=4;u64 buffer=read.generation_before,count=read.original_count;
    struct ap_read_entry_snapshot entry={0};
    assert(ap_read_entry_copy(&read,&nr,&fd,&buffer,&count,mutant_copy,&entry));
    for(unsigned nonnull=0;nonnull<2;nonnull++) {
        call=mutant_read_call();
        assert(ap_read_selection_enter(&call,&read,19,23,0x2000,29,14,0x6000));
        assert(ap_read_selection_session_post(
            &call,&read,19,23,0x2000,29,nonnull?0x8001:0));
    }
    return 0;
}
#else
#include <assert.h>
#include <stdio.h>
#include <string.h>
#include "fd-effects.h"

#include "connect-copy.h"
#include "fdupfd.h"

static unsigned copy_checks;
#define COPY_CHECK(x) do {assert(x);copy_checks++;} while(0)
static struct ap_task_command copy_owner(void) {
    return (struct ap_task_command){.provider=3,.command=7,.operation=AP_ORIGINAL_CONNECT,
        .expected_object=11,.generation_before=0x4000,.generation_after=17,
        .expected_level=4,.expected_option=24};
}
static struct ap_fd_call copy_selected(void) {
    return (struct ap_fd_call){.command=7,.function_ip=0x1000,.operation=AP_ORIGINAL_CONNECT,
        .raw_table=0x2000,
        .selection={.entered=1,.returned=1,.word=0x8001},
        .original.selection={.command=7,.call=11,.owner_mm=17,.provider=3,.task=19,.task_start=23,
            .table=29,.file=31,.user_address=0x4000,.fdput_flags=1,.ready=1,.requested_fd=4,.address_length=24}};
}
static int copy_begin(struct ap_fd_call *c,struct ap_task_command *o) {
    return ap_original_copy_begin(c,o,19,23,0x6000,0x6000,0x4000,24,0x8001,24,0x4000);
}
static int copy_end(struct ap_fd_call *c,struct ap_task_command *o,u64 remaining) {
    return ap_original_copy_end(c,o,19,23,0x6000,remaining,0x8001,24,0x4000);
}
static void copy_refuses_begin(struct ap_fd_call c,struct ap_task_command o) {
    struct ap_fd_call before=c;COPY_CHECK(!copy_begin(&c,&o));COPY_CHECK(!memcmp(&before,&c,sizeof(c)));
}
static void original_copy_controls(void) {
    // Referenced target loader is intentionally not executed by this control.
    (void)ap_require_retirement_target;
    struct ap_task_command o=copy_owner();struct ap_fd_call c=copy_selected();
    COPY_CHECK(copy_begin(&c,&o));COPY_CHECK(c.copied_address==0x6000 && c.original.copy_entered==1);
    struct ap_fd_call pending=c;COPY_CHECK(!copy_begin(&c,&o));COPY_CHECK(!memcmp(&c,&pending,sizeof(c)));
    COPY_CHECK(copy_end(&c,&o,0));COPY_CHECK(c.original.copy_returned==1 && !c.original.copy_remaining);
    struct ap_fd_call done=c;COPY_CHECK(!copy_end(&c,&o,0));COPY_CHECK(!memcmp(&c,&done,sizeof(c)));
    c=copy_selected();COPY_CHECK(!copy_end(&c,&o,0));COPY_CHECK(!c.original.copy_entered && !c.original.copy_returned);
    c.original.returned=-14;COPY_CHECK(ap_original_path(&c.original)==AP_ORIGINAL_UNKNOWN);
    c=copy_selected();COPY_CHECK(copy_begin(&c,&o));COPY_CHECK(copy_end(&c,&o,3));
    COPY_CHECK(c.original.copy_remaining==3 && c.original.copy_returned==1);
    COPY_CHECK(ap_original_path(&c.original)==AP_ORIGINAL_UNKNOWN); // no actual native result yet
    c.original.returned=-14;COPY_CHECK(ap_original_path(&c.original)==AP_ORIGINAL_COPY_FAULT);
    c=copy_selected();COPY_CHECK(copy_begin(&c,&o));pending=c;COPY_CHECK(!copy_end(&c,&o,25));COPY_CHECK(!memcmp(&c,&pending,sizeof(c)));
    c=copy_selected();c.selection.word=0x8000;c.original.selection.fdput_flags=0;
    COPY_CHECK(ap_original_copy_begin(&c,&o,19,23,0x6000,0x6000,0x4000,24,0x8000,24,0x4000));
    COPY_CHECK(ap_original_copy_end(&c,&o,19,23,0x6000,0,0x8000,24,0x4000)); // files->count==1
    c=copy_selected();c.original.selection.task_start++;copy_refuses_begin(c,o);
    c=copy_selected();c.original.selection.task++;copy_refuses_begin(c,o);
    c=copy_selected();c.original.selection.owner_mm++;copy_refuses_begin(c,o);
    c=copy_selected();c.command++;copy_refuses_begin(c,o);
    c=copy_selected();c.original.selection.call++;copy_refuses_begin(c,o);
    c=copy_selected();c.original.selection.provider++;copy_refuses_begin(c,o);
    c=copy_selected();c.original.selection.file=0;copy_refuses_begin(c,o);
    c=copy_selected();c.original.selection.ready=0;copy_refuses_begin(c,o);
    c=copy_selected();c.selection.returned=0;copy_refuses_begin(c,o);
    c=copy_selected();c.original.selection.address_length=0;o.expected_option=0;copy_refuses_begin(c,o);o=copy_owner();
    c=copy_selected();c.original.selection.address_length=129;o.expected_option=129;copy_refuses_begin(c,o);o=copy_owner();
    c=copy_selected();c.original.audit_entered=1;copy_refuses_begin(c,o);
    c=copy_selected();c.original.security_entered=1;copy_refuses_begin(c,o);
    c=copy_selected();c.original.problem=AP_FD_MISSING;copy_refuses_begin(c,o);
    c=copy_selected();c.original.complete=1;copy_refuses_begin(c,o);
    c=copy_selected();COPY_CHECK(!ap_original_copy_begin(&c,&o,19,23,0x6000,0x6001,0x4000,24,0x8001,24,0x4000));
    COPY_CHECK(!ap_original_copy_begin(&c,&o,19,23,0x6000,0x6000,0x4001,24,0x8001,24,0x4000));
    COPY_CHECK(!ap_original_copy_begin(&c,&o,19,23,0x6000,0x6000,0x4000,23,0x8001,24,0x4000));
    COPY_CHECK(copy_begin(&c,&o));pending=c;
    COPY_CHECK(!ap_original_copy_end(&c,&o,19,23,0x6001,0,0x8001,24,0x4000));
    COPY_CHECK(!ap_original_copy_end(&c,&o,19,23,0x6000,0,0x9001,24,0x4000));
    COPY_CHECK(!ap_original_copy_end(&c,&o,19,23,0x6000,0,0x8001,25,0x4000));
    COPY_CHECK(!ap_original_copy_end(&c,&o,19,23,0x6000,0,0x8001,24,0x4001));
    COPY_CHECK(!memcmp(&c,&pending,sizeof(c)));
    c=copy_selected();COPY_CHECK(ap_original_copy_infer_success(
        &c,&o,19,23,0x5ff8,0x6000,0x8001,24,0x4000));
    COPY_CHECK(c.copied_address==0x6000 && c.original.copy_entered==1 &&
        c.original.copy_returned==1 && !c.original.copy_remaining);
    done=c;COPY_CHECK(!ap_original_copy_infer_success(
        &c,&o,19,23,0x5ff8,0x6000,0x8001,24,0x4000));COPY_CHECK(!memcmp(&c,&done,sizeof(c)));
    c=copy_selected();pending=c;COPY_CHECK(!ap_original_copy_infer_success(
        &c,&o,19,23,0x5ff0,0x6000,0x8001,24,0x4000));COPY_CHECK(!memcmp(&c,&pending,sizeof(c)));
    c=copy_selected();pending=c;COPY_CHECK(!ap_original_copy_infer_success(
        &c,&o,19,23,~0ULL-7,0,0x8001,24,0x4000));COPY_CHECK(!memcmp(&c,&pending,sizeof(c)));
    COPY_CHECK(ap_shared_fdget_role(AP_ORIGINAL_CONNECT,0x1000,0x101c)==AP_SHARED_FDGET_CONNECT_ROLE);
    COPY_CHECK(ap_shared_fdget_role(AP_ACCEPT_EFFECT,0x1000,0x1021)==AP_SHARED_FDGET_ACCEPT_ROLE);
    COPY_CHECK(ap_shared_fdget_role(AP_ORIGINAL_EPOLL_CTL,0x1000,0x1023)==AP_SHARED_FDGET_EPOLL_PRIMARY_ROLE);
    COPY_CHECK(ap_shared_fdget_role(AP_ORIGINAL_EPOLL_CTL,0x1000,0x1037)==AP_SHARED_FDGET_EPOLL_TARGET_ROLE);
    COPY_CHECK(!ap_shared_fdget_role(AP_ORIGINAL_CONNECT,0x1000,0x1021));
    COPY_CHECK(ap_file_fdget_caller_site(0xffffffff81faaca0ULL,0xffffffff81facb54ULL));
    COPY_CHECK(!ap_file_fdget_caller_site(0xffffffff81faaca0ULL,0xffffffff81facb55ULL));
    COPY_CHECK(ap_file_fdget_session_marker(
        0xffffffff81faaca0ULL,0xffffffff81faaca6ULL));
    COPY_CHECK(!ap_file_fdget_session_marker(
        0xffffffff81faaca6ULL,0xffffffff81faaca6ULL));
    COPY_CHECK(!ap_file_fdget_session_marker(0,0xffffffff81faaca6ULL));
    COPY_CHECK(!ap_file_fdget_session_marker(1,AP_FILE_ENTRY_OFFSET+1));
    COPY_CHECK(ap_read_fdget_caller_site(0xffffffff81faede0ULL,0xffffffff81fae8b5ULL));
    COPY_CHECK(!ap_read_fdget_caller_site(0xffffffff81faede0ULL,0xffffffff81fae8b6ULL));
    c=copy_selected();c.selection.returned=0;c.selection.word=0;
    c.original.selection.ready=0;c.entry_stack=0x7000;
    COPY_CHECK(ap_fdget_session_post(&c,&o,0,19,23,0x2000,29,
        AP_SHARED_FDGET_CONNECT_ROLE,0x8001));
    COPY_CHECK(c.selection.returned==1 && c.selection.word==0x8001 && !c.entry_stack);
    c=copy_selected();c.selection.returned=0;c.selection.word=0;
    c.original.selection.ready=0;c.entry_stack=0x7000;pending=c;
    COPY_CHECK(!ap_fdget_session_post(&c,&o,0,19,23,0x2000,29,
        AP_SHARED_FDGET_ACCEPT_ROLE,0x8001));COPY_CHECK(!memcmp(&c,&pending,sizeof(c)));
    COPY_CHECK(!ap_fdget_session_post(&c,&o,0,20,23,0x2000,29,
        AP_SHARED_FDGET_CONNECT_ROLE,0x8001) && !memcmp(&c,&pending,sizeof(c)));
    COPY_CHECK(!ap_fdget_session_post(&c,&o,0,19,24,0x2000,29,
        AP_SHARED_FDGET_CONNECT_ROLE,0x8001) && !memcmp(&c,&pending,sizeof(c)));
    COPY_CHECK(!ap_fdget_session_post(&c,&o,0,19,23,0x2001,29,
        AP_SHARED_FDGET_CONNECT_ROLE,0x8001) && !memcmp(&c,&pending,sizeof(c)));
    COPY_CHECK(!ap_fdget_session_post(&c,&o,0,19,23,0x2000,30,
        AP_SHARED_FDGET_CONNECT_ROLE,0x8001) && !memcmp(&c,&pending,sizeof(c)));
    COPY_CHECK(!ap_fdget_session_post(&c,&o,0,19,23,0x2000,29,
        AP_SHARED_FDGET_CONNECT_ROLE,0x8002) && !memcmp(&c,&pending,sizeof(c)));
    // Both actual link roles, immutable program identity and positive metadata.
    struct bpf_prog_info p={.type=BPF_PROG_TYPE_KPROBE,.id=37};
    struct bpf_link_info l={.type=BPF_LINK_TYPE_PERF_EVENT,.id=41,.prog_id=37};
    l.perf_event.type=BPF_PERF_EVENT_KPROBE;l.perf_event.kprobe.name_len=sizeof("__sys_connect");
    l.perf_event.kprobe.offset=0x41;l.perf_event.kprobe.cookie=3;
    COPY_CHECK(ap_copy_link_matches(0,&p,sizeof(p),&l,sizeof(l),"__sys_connect"));
    COPY_CHECK(!ap_copy_link_matches(1,&p,sizeof(p),&l,sizeof(l),"__sys_connect"));
    l.perf_event.kprobe.offset=0x46;l.perf_event.kprobe.cookie=5;
    COPY_CHECK(ap_copy_link_matches(1,&p,sizeof(p),&l,sizeof(l),"__sys_connect"));
    COPY_CHECK(!ap_copy_link_matches(2,&p,sizeof(p),&l,sizeof(l),"__sys_connect"));
    COPY_CHECK(!ap_copy_link_matches(1,&p,0,&l,sizeof(l),"__sys_connect"));
    COPY_CHECK(!ap_copy_link_matches(1,&p,sizeof(p),&l,0,"__sys_connect"));
    COPY_CHECK(!ap_copy_link_matches(1,&p,sizeof(p),&l,sizeof(l),"__bad_connect"));
    struct bpf_link_info good=l;
#define REJECT_LINK(field,value) do {l=good;l.field=value;COPY_CHECK(!ap_copy_link_matches(1,&p,sizeof(p),&l,sizeof(l),"__sys_connect"));} while(0)
    REJECT_LINK(type,BPF_LINK_TYPE_KPROBE_MULTI);REJECT_LINK(id,0);REJECT_LINK(prog_id,38);
    REJECT_LINK(perf_event.type,BPF_PERF_EVENT_KRETPROBE);REJECT_LINK(perf_event.kprobe.name_len,12);
    REJECT_LINK(perf_event.kprobe.offset,0x47);REJECT_LINK(perf_event.kprobe.cookie,3);
    REJECT_LINK(perf_event.kprobe.missed,1);l=good;
    p.recursion_misses=1;COPY_CHECK(!ap_copy_link_matches(1,&p,sizeof(p),&l,sizeof(l),"__sys_connect"));
    p.recursion_misses=0;p.id=0;COPY_CHECK(!ap_copy_link_matches(1,&p,sizeof(p),&l,sizeof(l),"__sys_connect"));
    p.id=37;p.type=BPF_PROG_TYPE_TRACING;COPY_CHECK(!ap_copy_link_matches(1,&p,sizeof(p),&l,sizeof(l),"__sys_connect"));
    assert(copy_checks==108);
    printf("original copy boundary production controls=%u\n",copy_checks);
}
#undef COPY_CHECK
#undef REJECT_LINK


static unsigned fdget_checks;
#define FDGET_CHECK(x) do {assert(x);fdget_checks++;} while(0)
struct fdget_fixture {
    struct ap_task_command owner;
    struct ap_fd_call call;
    struct ap_fd_accept accept;
    unsigned connect;
};
static struct fdget_fixture fdget_fixture(unsigned connect) {
    struct fdget_fixture f={.owner=copy_owner(),.call=copy_selected(),.connect=connect};
    f.call.selection=(struct ap_fd_selection){0};f.call.original.selection.ready=0;
    f.call.original.selection.file=0;f.call.original.selection.fdput_flags=0;f.call.raw_table=0x9000;
    if(!connect) {
        f.owner.operation=f.call.operation=AP_ACCEPT_EFFECT;
        f.owner.expected_option=0;
        f.accept=(struct ap_fd_accept){.command=7,.task=19,.task_start=23,.table=29,
            .phases=AP_FD_ENTERED,.requested_fd=4};
    }
    return f;
}
static int fdget_enter(struct fdget_fixture *f) {
    return ap_fdget_entry(&f->call,&f->owner,f->connect?NULL:&f->accept,
        19,23,0x9000,29,f->connect?AP_CONNECT_ENTRY_COOKIE:AP_ACCEPT_ENTRY_COOKIE,0x6100);
}
static int fdget_post(struct fdget_fixture *f,u64 word) {
    return ap_fdget_post(&f->call,&f->owner,f->connect?NULL:&f->accept,
        19,23,0x9000,29,f->connect?AP_CONNECT_POST_COOKIE:AP_ACCEPT_POST_COOKIE,
        0x6100-(f->connect?0xa0:0x40),f->owner.expected_option,0x4000,word);
}
static void original_fdget_controls(void) {
    const u64 words[]={0,0x8000,0x8001};
    for(unsigned connect=0;connect<2;connect++) {
        struct fdget_fixture f=fdget_fixture(connect),old=f;
        FDGET_CHECK(!fdget_post(&f,0x8000));FDGET_CHECK(!memcmp(&f,&old,sizeof(f)));
        for(unsigned w=0;w<3;w++) {
            f=fdget_fixture(connect);FDGET_CHECK(fdget_enter(&f));
            FDGET_CHECK(f.call.selection.entered==1 && !f.call.selection.returned &&
                !f.call.selection.word && f.call.entry_stack==0x6100);
            old=f;FDGET_CHECK(!fdget_enter(&f));FDGET_CHECK(!memcmp(&f,&old,sizeof(f)));
            FDGET_CHECK(fdget_post(&f,words[w]));
            FDGET_CHECK(ap_fd_selection_complete(&f.call.selection) &&
                f.call.selection.word==words[w] && !f.call.entry_stack);
            old=f;FDGET_CHECK(!fdget_post(&f,words[w]));FDGET_CHECK(!memcmp(&f,&old,sizeof(f)));
        }
        for(unsigned bad=0;bad<13;bad++) {
            f=fdget_fixture(connect);
            u64 task=19,start=23,raw=0x9000,table=29,cookie=connect?7:6,stack=0x6100;
            switch(bad) {
            case 0:f.owner.command++;break;
            case 1:f.call.command++;break;
            case 2:f.owner.provider=0;break;
            case 3:task++;break;
            case 4:start++;break;
            case 5:raw++;break;
            case 6:table++;break;
            case 7:cookie=AP_FDGET_COOKIE;break; /* obsolete global fdget role */
            case 8:stack=0;break;
            case 9:f.call.function_ip=0;break;
            case 10:f.owner.expected_level++;break;
            case 11:f.owner.expected_option++;break;
            case 12:f.call.entry_stack=stack;break;
            }
            old=f;
            FDGET_CHECK(!ap_fdget_entry(&f.call,&f.owner,connect?NULL:&f.accept,
                task,start,raw,table,cookie,stack));
            FDGET_CHECK(!memcmp(&f,&old,sizeof(f)));
        }
        for(unsigned bad=0;bad<21;bad++) {
            f=fdget_fixture(connect);FDGET_CHECK(fdget_enter(&f));
            u64 task=19,start=23,raw=0x9000,table=29,cookie=connect?9:8;
            u64 stack=0x6100-(connect?0xa0:0x40),option=f.owner.expected_option,address=0x4000,word=0x8001;
            switch(bad) {
            case 0:f.call.selection.entered=0;break;
            case 1:cookie=connect?8:9;break;
            case 2:stack++;break;
            case 3:option++;break;
            case 4:raw++;break;
            case 5:table++;break;
            case 6:task++;break;
            case 7:start++;break;
            case 8:f.owner.command++;break;
            case 9:f.call.command++;break;
            case 10:word=0x8002;break;
            case 11:word=1;break;
            case 12:f.call.selection.returned=1;break;
            case 13:if(connect)f.call.original.selection.provider++;else f.accept.problem=1;break;
            case 14:f.call.function_ip=0;break;
            case 15:f.owner.operation++;break;
            case 16:if(connect)f.call.original.selection.ready=1;else f.accept.phases|=AP_FD_LISTENER;break;
            case 17:f.owner.expected_level++;break;
            case 18:f.call.selection.word=0x8000;break;
            case 19:f.call.entry_stack=0;break;
            case 20:if(connect)address++;else f.accept.flags++;break;
            }
            old=f;
            FDGET_CHECK(!ap_fdget_post(&f.call,&f.owner,connect?NULL:&f.accept,
                task,start,raw,table,cookie,stack,option,address,word));
            FDGET_CHECK(!memcmp(&f,&old,sizeof(f)));
        }
    }
    for(unsigned which=0;which<2;which++) {
        struct bpf_prog_info p={.id=11,.type=BPF_PROG_TYPE_KPROBE};
        struct bpf_link_info l={.id=13,.type=BPF_LINK_TYPE_PERF_EVENT,.prog_id=11};
        l.perf_event.type=BPF_PERF_EVENT_KPROBE;
        l.perf_event.kprobe.name_len=which?sizeof("__sys_connect"):sizeof("__sys_accept4");
        l.perf_event.kprobe.offset=which?0x1c:0x21;l.perf_event.kprobe.cookie=which?9:8;
        const char *symbol=which?"__sys_connect":"__sys_accept4";
        FDGET_CHECK(ap_fdget_link_matches(which,&p,sizeof(p),&l,sizeof(l),symbol));
        for(unsigned bad=0;bad<17;bad++) {
            struct bpf_prog_info q=p;struct bpf_link_info m=l;
            unsigned ps=sizeof(p),ls=sizeof(l),role=which;const char *name=symbol;
            const char wrong_symbol[64]="wrong_symbol";
            switch(bad) {
            case 0:role=2;break;
            case 1:ps=offsetof(struct bpf_prog_info,recursion_misses);break;
            case 2:ls=offsetof(struct bpf_link_info,perf_event.kprobe.cookie);break;
            case 3:q.type=BPF_PROG_TYPE_TRACING;break;
            case 4:q.id=0;break;
            case 5:m.id=0;break;
            case 6:m.type=BPF_LINK_TYPE_KPROBE_MULTI;break;
            case 7:m.prog_id++;break;
            case 8:m.perf_event.type=BPF_PERF_EVENT_KRETPROBE;break;
            case 9:m.perf_event.kprobe.name_len--;break;
            case 10:name=wrong_symbol;break;
            case 11:m.perf_event.kprobe.offset++;break;
            case 12:m.perf_event.kprobe.cookie++;break;
            case 13:q.recursion_misses=1;break;
            case 14:m.perf_event.kprobe.missed=1;break;
            case 15:role=1-which;break;
            case 16:m.perf_event.kprobe.name_len++;break;
            }
            FDGET_CHECK(!ap_fdget_link_matches(role,&q,ps,&m,ls,name));
        }
    }
    assert(fdget_checks==266);
    printf("original fdget boundary production controls=%u\n",fdget_checks);
}

/* Exercise the physical-frame adapter used by the actual post program. Only
 * probe_read_kernel is replaced: it reads one exact saved-RBP slot or fails.
 * The old direct-SP comparison is also required to refuse every valid nested
 * frame, so a passing abstract direct-frame fixture cannot mask this defect. */
static unsigned fdget_frame_checks,fdget_frame_reads;
static u64 fdget_frame_slot,fdget_frame_bp;
static long fdget_frame_error;
#define FRAME_CHECK(x) do {assert(x);fdget_frame_checks++;} while(0)
static long fdget_frame_read(void *to,u32 size,const void *from) {
    fdget_frame_reads++;
    if(size!=sizeof(fdget_frame_bp) || (u64)from!=fdget_frame_slot)return -14;
    memcpy(to,&fdget_frame_bp,sizeof(fdget_frame_bp));
    return fdget_frame_error;
}
static u64 fdget_frame_setup(unsigned connect,u64 locals) {
    const u64 frame=connect?AP_CONNECT_ENTRY_FRAME_BYTES:AP_ACCEPT_ENTRY_FRAME_BYTES;
    fdget_frame_bp=0x6100-8;
    const u64 body_entry=fdget_frame_bp-locals-8;
    fdget_frame_slot=body_entry-8;fdget_frame_error=0;fdget_frame_reads=0;
    return body_entry-frame;
}
static int fdget_physical_post(struct fdget_fixture *f,u64 physical_stack,u64 word) {
    const u64 stack=ap_fdget_post_stack(f->owner.operation,physical_stack,fdget_frame_read);
    return ap_fdget_post(&f->call,&f->owner,f->connect?NULL:&f->accept,
        19,23,0x9000,29,f->connect?AP_CONNECT_POST_COOKIE:AP_ACCEPT_POST_COOKIE,
        stack,f->owner.expected_option,0x4000,word);
}
static void original_fdget_frame_controls(void) {
    const u64 locals[]={0x38,0x78,0x108},words[]={0,0x8000,0x8001};
    for(unsigned connect=0;connect<2;connect++) {
        for(unsigned n=0;n<3;n++)for(unsigned w=0;w<3;w++) {
            struct fdget_fixture f=fdget_fixture(connect);
            u64 physical=fdget_frame_setup(connect,locals[n]);
            FRAME_CHECK(fdget_enter(&f));
            struct fdget_fixture old=f;
            FRAME_CHECK(!ap_fdget_post(&f.call,&f.owner,connect?NULL:&f.accept,
                19,23,0x9000,29,connect?AP_CONNECT_POST_COOKIE:AP_ACCEPT_POST_COOKIE,
                physical,f.owner.expected_option,0x4000,words[w]));
            FRAME_CHECK(!memcmp(&f,&old,sizeof(f)));
            FRAME_CHECK(fdget_physical_post(&f,physical,words[w]));
            FRAME_CHECK(fdget_frame_reads==1);
            FRAME_CHECK(ap_fd_selection_complete(&f.call.selection) &&
                f.call.selection.word==words[w] && !f.call.entry_stack);
        }
        for(unsigned bad=0;bad<22;bad++) {
            struct fdget_fixture f=fdget_fixture(connect);
            u64 physical=fdget_frame_setup(connect,0x78),word=0x8001;
            FRAME_CHECK(fdget_enter(&f));
            switch(bad) {
            case 0:fdget_frame_slot++;break; /* missing exact slot */
            case 1:fdget_frame_error=-14;break;
            case 2:fdget_frame_error=-5;break; /* valid payload cannot hide failed read */
            case 3:fdget_frame_bp=0;break;
            case 4:fdget_frame_bp+=8;break; /* wrong outer frame */
            case 5:fdget_frame_bp-=8;break; /* CALL/IPMODIFY frame, not admitted JMP */
            case 6:physical++;break;
            case 7:physical+=8;break;
            case 8:physical=0;break;
            case 9:physical=~0ULL-7;break; /* slot calculation overflows */
            case 10:fdget_frame_bp=~0ULL-7;break; /* reconstructed entry overflows */
            case 11:fdget_frame_bp=fdget_frame_slot+8;break;
            case 12:fdget_frame_bp=8;break; /* below body and reconstruction frame */
            case 13:fdget_frame_bp++;break;
            case 14:f.call.command++;break;
            case 15:if(connect)f.call.original.selection.task++;else f.accept.task++;break;
            case 16:if(connect)f.call.original.selection.task_start++;else f.accept.task_start++;break;
            case 17:if(connect)f.call.original.selection.table++;else f.accept.table++;break;
            case 18:f.call.entry_stack=0;break;
            case 19:f.call.selection.entered=0;break;
            case 20:word=0x8002;break;
            case 21:fdget_frame_slot+=connect?0x40:0xa0;break; /* wrong body frame */
            }
            struct fdget_fixture old=f;
            FRAME_CHECK(!fdget_physical_post(&f,physical,word));
            FRAME_CHECK(!memcmp(&f,&old,sizeof(f)));
        }
    }
    assert(fdget_frame_checks==240);
    printf("original fdget physical frame controls=%u\n",fdget_frame_checks);
}
#undef FRAME_CHECK

/* Additive ABI6 field controls; the original birth/return/actor cases remain. */
static void native_clear_tid_controls(const struct ap_task_command *command,
                                     const struct ap_native_birth *source) {
    unsigned checks=0;
#define CLEARTID_CHECK(v) do {assert(v);checks++;} while(0)
    struct ap_native_birth birth=*source;birth.ready=1;birth.kernel_flags=0;
    birth.clear_child_tid=0;CLEARTID_CHECK(ap_native_birth_matches(command,&birth));
    birth.clear_child_tid=0x1234;CLEARTID_CHECK(!ap_native_birth_matches(command,&birth));
    birth.kernel_flags=AP_CLONE_CHILD_CLEARTID;birth.clear_child_tid=0;
    CLEARTID_CHECK(ap_native_birth_matches(command,&birth));
    birth.clear_child_tid=0x1234;CLEARTID_CHECK(ap_native_birth_matches(command,&birth));
    /* CHILD_SETTID alone never supplies a clear-child-TID field. */
    birth.kernel_flags=0x1000000ULL;CLEARTID_CHECK(!ap_native_birth_matches(command,&birth));
    birth.clear_child_tid=0;CLEARTID_CHECK(ap_native_birth_matches(command,&birth));
    /* Linux retains the full pointer scalar; do not truncate or preread it. */
    birth.kernel_flags=AP_CLONE_CHILD_CLEARTID;birth.clear_child_tid=0xffff000012341234ULL;
    CLEARTID_CHECK(ap_native_birth_matches(command,&birth));
    birth.kernel_flags=0;CLEARTID_CHECK(!ap_native_birth_matches(command,&birth));
    assert(checks==8);printf("actual clear-child-TID predicates: %u checks\n",checks);
#undef CLEARTID_CHECK
}


/* Same production predicates as the original kernel interval callbacks. These
 * do not stand in for the required native fast/resize/shared-table fixture. */
static void fdupfd_interval_controls(void) {
    unsigned checks=0;
#define FDUP_CHECK(v) do {assert(v);checks++;} while(0)
    const struct ap_fdupfd entered={.function_ip=0x1000,.entry_stack=0x8000,
        .raw_table=0x4000,.minimum=5,.flags=0x80000};
    struct ap_fdupfd s=entered;
    FDUP_CHECK(ap_fdupfd_allocation_enter(&s,0x102e,0x7000,0x4000,5,0x80000));
    FDUP_CHECK(ap_fdupfd_allocation_return(&s,0x7000,0x4000,5));
    FDUP_CHECK(ap_fdupfd_completion(&s,0x8000,0x4000,5,19)==1);
    FDUP_CHECK(ap_fdupfd_completion(&s,0x8000,0x4000,5,0)==-1);
    FDUP_CHECK(ap_fdupfd_completion(&s,0x8000,0x4000,6,19)==-1);
    FDUP_CHECK(ap_fdupfd_completion(&s,0x8001,0x4000,5,19)==-1);
    FDUP_CHECK(ap_fdupfd_completion(&s,0x8000,0x4001,5,19)==-1);
    struct ap_fdupfd done=s;
    FDUP_CHECK(!ap_fdupfd_allocation_return(&s,0x7000,0x4000,5));
    FDUP_CHECK(!memcmp(&s,&done,sizeof(s)));
    FDUP_CHECK(!ap_fdupfd_allocation_enter(&s,0x102e,0x7000,0x4000,5,0x80000));
    FDUP_CHECK(!memcmp(&s,&done,sizeof(s)));
    /* Missing allocator cannot turn a positive or unrelated error into fact. */
    FDUP_CHECK(ap_fdupfd_completion(&entered,0x8000,0x4000,5,0)==-1);
    FDUP_CHECK(ap_fdupfd_completion(&entered,0x8000,0x4000,-22,0)==0);
    FDUP_CHECK(ap_fdupfd_completion(&entered,0x8000,0x4000,-12,0)==-1);
    FDUP_CHECK(ap_fdupfd_completion(&entered,0x8000,0x4000,-22,19)==-1);
    for(unsigned flags=0;flags<=0x80000;flags+=0x80000) {
        s=entered;s.flags=flags;
        FDUP_CHECK(ap_fdupfd_allocation_enter(&s,0x102e,0x7000,0x4000,5,flags));
        struct ap_fdupfd pending=s;
        FDUP_CHECK(ap_fdupfd_completion(&s,0x8000,0x4000,5,19)==-1);
        FDUP_CHECK(!ap_fdupfd_allocation_return(&s,0x7001,0x4000,5));
        FDUP_CHECK(!ap_fdupfd_allocation_return(&s,0x7000,0x4001,5));
        FDUP_CHECK(!ap_fdupfd_allocation_return(&s,0x7000,0x4000,4));
        FDUP_CHECK(!ap_fdupfd_allocation_return(&s,0x7000,0x4000,-4096));
        FDUP_CHECK(!memcmp(&s,&pending,sizeof(s)));
        FDUP_CHECK(ap_fdupfd_allocation_return(&s,0x7000,0x4000,1024));
        FDUP_CHECK(ap_fdupfd_completion(&s,0x8000,0x4000,1024,19)==1);
        if(flags)break;
    }
    /* Both real alloc_fd error families retain exact errno with no interval. */
    const int errors[]={-24,-12};
    for(unsigned i=0;i<2;i++) {
        s=entered;
        FDUP_CHECK(ap_fdupfd_allocation_enter(&s,0x102e,0x7000,0x4000,5,0x80000));
        FDUP_CHECK(ap_fdupfd_allocation_return(&s,0x7000,0x4000,errors[i]));
        FDUP_CHECK(ap_fdupfd_completion(&s,0x8000,0x4000,errors[i],0)==0);
        FDUP_CHECK(ap_fdupfd_completion(&s,0x8000,0x4000,errors[i],19)==-1);
        FDUP_CHECK(ap_fdupfd_completion(&s,0x8000,0x4000,-9,0)==-1);
    }
    /* Exact direct caller/flags/owner; refusal never mutates retained state. */
    const u64 sites[]={0,0x102d,0x102f,0x202e};
    for(unsigned i=0;i<4;i++) {
        s=entered;FDUP_CHECK(!ap_fdupfd_allocation_enter(&s,sites[i],0x7000,0x4000,5,0x80000));
        FDUP_CHECK(!memcmp(&s,&entered,sizeof(s)));
    }
    s=entered;
    FDUP_CHECK(!ap_fdupfd_allocation_enter(&s,0x102e,0,0x4000,5,0x80000));
    FDUP_CHECK(!ap_fdupfd_allocation_enter(&s,0x102e,0x7000,0,5,0x80000));
    FDUP_CHECK(!ap_fdupfd_allocation_enter(&s,0x102e,0x7000,0x4001,5,0x80000));
    FDUP_CHECK(!ap_fdupfd_allocation_enter(&s,0x102e,0x7000,0x4000,4,0x80000));
    FDUP_CHECK(!ap_fdupfd_allocation_enter(&s,0x102e,0x7000,0x4000,5,0));
    FDUP_CHECK(!memcmp(&s,&entered,sizeof(s)));
    FDUP_CHECK(!ap_fdupfd_alloc_site(0,0x2e));
    FDUP_CHECK(!ap_fdupfd_alloc_site(~0ULL-0x2d,0));
    FDUP_CHECK(ap_fdupfd_alloc_site(~0ULL-0x2e,~0ULL));
    s=entered;s.entry_stack=0;
    FDUP_CHECK(!ap_fdupfd_allocation_enter(&s,0x102e,0x7000,0x4000,5,0x80000));
    assert(checks==61);printf("f_dupfd original allocation interval controls=%u\n",checks);
#undef FDUP_CHECK
}

/* Production entry/post/result predicates; scalar inputs are controls,
 * not claims that a native kernel observer ran. Old controls are unchanged. */
static struct ap_task_command file_owner(void) {
    return (struct ap_task_command){.provider=3,.command=7,.operation=AP_ORIGINAL_FILE,
        .expected_object=11,.generation_before=72,.generation_after=17,.expected_level=4,.expected_option=3};
}
static struct ap_fd_call file_entering(void) {
    return (struct ap_fd_call){.command=7,.file_entry_ip=0x1000,.raw_table=0x2000,.operation=AP_ORIGINAL_FILE,
        .original.selection={.command=7,.call=11,.owner_mm=17,.provider=3,.task=19,.task_start=23,
            .table=29,.user_address=72,.requested_fd=4,.address_length=3}};
}
static void original_file_controls(void) {
    unsigned checks=0;
#define FILE_CHECK(x) do {assert(x);checks++;} while(0)
    struct ap_task_command owner=file_owner();
    FILE_CHECK(ap_original_file_shape(72,3));
    FILE_CHECK(!ap_original_file_shape(71,3));
    FILE_CHECK(!ap_original_file_shape(72,4));
    const u64 words[]={0,0x8000,0x8001};
    for(unsigned i=0;i<3;i++) {
        struct ap_fd_call c=file_entering(),before=c;
        FILE_CHECK(!ap_file_selection_post(&c,&owner,19,23,0x2000,29,13,0x5fe8,4,3,words[i],0x1077));
        FILE_CHECK(!memcmp(&c,&before,sizeof(c)));
        FILE_CHECK(ap_file_selection_enter(&c,&owner,19,23,0x2000,29,12,0x6000));
        FILE_CHECK(!ap_file_selection_enter(&c,&owner,19,23,0x2000,29,12,0x6000));
        FILE_CHECK(ap_file_selection_post(&c,&owner,19,23,0x2000,29,13,0x5fe8,4,3,words[i],0x1077));
        FILE_CHECK(ap_fd_selection_complete(&c.selection) && c.selection.word==words[i] && !c.entry_stack);
        before=c;
        FILE_CHECK(!ap_file_selection_post(&c,&owner,19,23,0x2000,29,13,0x5fe8,4,3,words[i],0x1077) && !memcmp(&c,&before,sizeof(c)));
    }
    {
        struct ap_fd_call c=file_entering();
        FILE_CHECK(ap_file_selection_enter(&c,&owner,19,23,0x2000,29,12,0x6000));
        FILE_CHECK(ap_file_selection_session_post(&c,&owner,19,23,0x2000,29,0x8001));
        FILE_CHECK(ap_fd_selection_complete(&c.selection) && c.selection.word==0x8001 && !c.entry_stack);
        struct ap_fd_call before=c;
        FILE_CHECK(!ap_file_selection_session_post(&c,&owner,19,23,0x2000,29,0x8001));
        FILE_CHECK(!memcmp(&c,&before,sizeof(c)));
        c=file_entering();FILE_CHECK(ap_file_selection_enter(&c,&owner,19,23,0x2000,29,12,0x6000));
        before=c;FILE_CHECK(!ap_file_selection_session_post(&c,&owner,19,23,0x2000,30,0x8001));
        FILE_CHECK(!memcmp(&c,&before,sizeof(c)));
    }
    for(unsigned bad=0;bad<16;bad++) {
        struct ap_task_command o=owner;struct ap_fd_call c=file_entering();
        FILE_CHECK(ap_file_selection_enter(&c,&o,19,23,0x2000,29,12,0x6000));
        u64 task=19,start=23,table=29,raw_table=0x2000,cookie=13,stack=0x5fe8,fd=4,command=3,word=0x8001;
        if(bad==0)fd++;if(bad==1)command++;if(bad==2)cookie=9;if(bad==3)stack++;
        if(bad==4)stack+=8;if(bad==5)o.provider++;if(bad==6)o.expected_object++;if(bad==7)o.generation_after++;
        if(bad==8)task++;if(bad==9)start++;if(bad==10)raw_table++;if(bad==11)table++;
        if(bad==12)word=0x8002;if(bad==13)word=1;
        if(bad==14)fd|=1ULL<<32;if(bad==15)command|=1ULL<<32;
        struct ap_fd_call exact=c;
        FILE_CHECK(!ap_file_selection_post(&c,&o,task,start,raw_table,table,cookie,stack,fd,command,word,0x1077));
        FILE_CHECK(!memcmp(&exact,&c,sizeof(c)));
    }
    struct ap_fd_call c=file_entering();c.selection=(struct ap_fd_selection){1,1,0x8001};c.selected_file=31;
    c.original.selection.file=31;c.original.selection.fdput_flags=1;c.original.selection.ready=1;
    struct ap_command_result r={.command=7,.operation=AP_ORIGINAL_FILE,.task=19,.start_boottime=23,
        .identity={.provider=3},.phase=AP_COMMAND_RUNNING};
    const s64 returns[]={0,2048,0x7fffffffLL,-1,-9,-4095};
    for(unsigned i=0;i<6;i++) {
        FILE_CHECK(ap_original_file_native_return(&owner,&r,&c,19,23,72,4,3,returns[i]));
        struct ap_original_result result=c.original;result.complete=1;result.returned=(s32)returns[i];
        struct ap_command_result done=r;done.phase=AP_COMMAND_DONE;done.returned=(s32)returns[i];
        FILE_CHECK(ap_original_result_matches(&owner,&done,&result));
    }
    FILE_CHECK(!ap_original_file_native_return(&owner,&r,&c,19,23,72,4,3,-4096));
    FILE_CHECK(!ap_original_file_native_return(&owner,&r,&c,19,23,72,4,3,0x80000000LL));
    FILE_CHECK(!ap_original_file_native_return(&owner,&r,&c,19,23,71,4,3,0));
    FILE_CHECK(!ap_original_file_native_return(&owner,&r,&c,19,23,72,5,3,0));
    FILE_CHECK(!ap_original_file_native_return(&owner,&r,&c,19,23,72,4,4,0));
    FILE_CHECK(!ap_original_file_native_return(&owner,&r,&c,20,23,72,4,3,0));
    c.selected_file++;FILE_CHECK(!ap_original_file_native_return(&owner,&r,&c,19,23,72,4,3,0));c.selected_file--;
    c.selection.word=0;FILE_CHECK(!ap_original_file_native_return(&owner,&r,&c,19,23,72,4,3,0));c.selection.word=0x8001;
    c.original.selection.ready=0;
    FILE_CHECK(!ap_original_file_native_return(&owner,&r,&c,19,23,72,4,3,-9));
    c.original.selection.ready=1;c.selected_file=0;c.original.selection.file=0;
    c.original.selection.fdput_flags=0;c.selection.word=0;
    FILE_CHECK(ap_original_file_native_return(&owner,&r,&c,19,23,72,4,3,-9));
    FILE_CHECK(!ap_original_file_native_return(&owner,&r,&c,19,23,72,4,3,0));
    FILE_CHECK(!ap_original_file_native_return(&owner,&r,&c,19,23,72,4,3,-1));
    struct ap_original_result result=c.original;result.complete=1;result.returned=-9;
    r.phase=AP_COMMAND_DONE;r.returned=-9;
    FILE_CHECK(ap_original_result_matches(&owner,&r,&result));
    for(unsigned bad=0;bad<12;bad++) {
        struct ap_original_result b=result;struct ap_command_result q=r;
        if(bad==0)b.copy_entered=1;if(bad==1)b.copy_returned=1;if(bad==2)b.copy_remaining=1;
        if(bad==3)b.audit_entered=1;if(bad==4)b.audit_returned=1;if(bad==5)b.audit_result=-1;
        if(bad==6)b.security_entered=1;if(bad==7)b.security_returned=1;if(bad==8)b.security_result=-1;
        if(bad==9)b.address[0]=1;if(bad==10)b.reserved=1;if(bad==11)q.identity.provider++;
        FILE_CHECK(!ap_original_result_matches(&owner,&q,&b));
    }
    struct bpf_prog_info program={.type=BPF_PROG_TYPE_KPROBE,.id=17};
    struct bpf_link_info link={.type=BPF_LINK_TYPE_PERF_EVENT,.id=23,.prog_id=17};
    link.perf_event.type=BPF_PERF_EVENT_KPROBE;
    link.perf_event.kprobe.name_len=sizeof("fdget_raw");
    link.perf_event.kprobe.offset=0x7c;link.perf_event.kprobe.cookie=13;
    FILE_CHECK(ap_fdget_link_matches(2,&program,sizeof(program),&link,sizeof(link),"fdget_raw"));
    const char wrong_symbol[64]="__sys_connect";
    for(unsigned bad=0;bad<7;bad++) {
        struct bpf_prog_info p=program;struct bpf_link_info l=link;
        if(bad==0)p.recursion_misses=1;if(bad==1)l.perf_event.kprobe.missed=1;
        if(bad==2)l.perf_event.kprobe.offset=0x1c;if(bad==3)l.perf_event.kprobe.cookie=9;
        if(bad==4)l.prog_id++;if(bad==5)l.perf_event.kprobe.name_len--;
        FILE_CHECK(!ap_fdget_link_matches(2,&p,sizeof(p),&l,sizeof(l),bad==6?wrong_symbol:"fdget_raw"));
    }
    assert(checks==125);printf("original file selection/result: %u checks\n",checks);
#undef FILE_CHECK
}


/* These controls use the production pre-claim operand and classic-PC guards.
 * They do not assert that a native probe executed. The original 111 obligations remain above, plus six full-register checks. */
static void inline_file_selection_controls(void) {
    unsigned checks=0;
#define INLINE_CHECK(v) do { assert(v);checks++; } while(0)
    struct ap_task_command owner=file_owner();
    INLINE_CHECK(ap_file_entry_operands(&owner,4,72,4,3));
    owner.expected_level=-1;INLINE_CHECK(ap_file_entry_operands(&owner,0xffffffffULL,72,-1,3));
    owner=file_owner();INLINE_CHECK(!ap_file_entry_operands(NULL,4,72,4,3));
    for(unsigned bad=0;bad<9;bad++) {
        struct ap_task_command o=owner;u64 lookup_fd=4;s64 original=72;s32 fd=4,command=3;
        if(bad==0)lookup_fd=5;if(bad==1)original=71;if(bad==2)fd=5;if(bad==3)command=4;
        if(bad==4)o.operation=AP_ORIGINAL_CONNECT;if(bad==5)o.generation_before=73;
        if(bad==6)o.expected_option=4;if(bad==7)lookup_fd|=1ULL<<32;if(bad==8)original|=1LL<<32;
        INLINE_CHECK(!ap_file_entry_operands(&o,lookup_fd,original,fd,command));
    }
    /* Raw ctx.ip is site+1 for both int3 and optimized classic callbacks.
     * Literal delta0x77 comes from the installed body's two instructions. */
    INLINE_CHECK(ap_file_post_site(0x1001,0x1078));
    INLINE_CHECK(!ap_file_post_site(0,0x77));
    INLINE_CHECK(!ap_file_post_site(0x1001,0));
    INLINE_CHECK(!ap_file_post_site(0x1001,0x1077));
    INLINE_CHECK(!ap_file_post_site(0x1001,0x1079));
    INLINE_CHECK(!ap_file_post_site(0x1001,0x1031));
    INLINE_CHECK(!ap_file_post_site(0x1001,0x1000));
    INLINE_CHECK(ap_file_post_site(~0ULL-0x77,~0ULL));
    INLINE_CHECK(!ap_file_post_site(~0ULL-0x76,0));
    INLINE_CHECK(!ap_file_post_site(~0ULL,0x76));
    const u64 wrong_post[]={0,0x1076,0x1078,0x1000};
    for(unsigned i=0;i<4;i++) {
        struct ap_fd_call c=file_entering();
        INLINE_CHECK(ap_file_selection_enter(&c,&owner,19,23,0x2000,29,12,0x6000));
        struct ap_fd_call old=c;
        INLINE_CHECK(!ap_file_selection_post(&c,&owner,19,23,0x2000,29,13,0x5fe8,4,3,0x8000,wrong_post[i]));
        INLINE_CHECK(!memcmp(&old,&c,sizeof(c)));
    }
    struct bpf_prog_info p={.type=BPF_PROG_TYPE_KPROBE,.id=17};
    struct bpf_link_info l={.type=BPF_LINK_TYPE_PERF_EVENT,.id=23,.prog_id=17};
    l.perf_event.type=BPF_PERF_EVENT_KPROBE;l.perf_event.kprobe.name_len=sizeof("fdget_raw");
    l.perf_event.kprobe.offset=0x5;l.perf_event.kprobe.cookie=12;
    INLINE_CHECK(ap_fdget_link_matches(3,&p,sizeof(p),&l,sizeof(l),"fdget_raw"));
    for(unsigned bad=0;bad<7;bad++) {
        struct bpf_prog_info q=p;struct bpf_link_info m=l;
        if(bad==0)q.recursion_misses=1;if(bad==1)m.perf_event.kprobe.missed=1;
        if(bad==2)m.perf_event.kprobe.offset=0x7c;if(bad==3)m.perf_event.kprobe.cookie=13;
        if(bad==4)m.prog_id++;if(bad==5)m.perf_event.kprobe.name_len--;
        INLINE_CHECK(!ap_fdget_link_matches(3,&q,sizeof(q),&m,sizeof(m),bad==6?"__se_sys_fcntl":"fdget_raw"));
    }
    INLINE_CHECK(offsetof(struct ap_fd_call,file_entry_ip)==offsetof(struct ap_fd_call,function_ip));
    assert(checks==43);printf("inline original file operands/PC/link guards: %u checks\n",checks);
#undef INLINE_CHECK
}

/* Exercise the production kernel-read adapter, substituting only its lower
 * copy primitive. These controls do not manufacture a native probe receipt. */
static struct { s64 original_nr; u64 fd, file_command; } file_read_regs;
static unsigned file_read_calls,file_read_fail;
static long file_read_error;
static long file_entry_read(void *to,u32 size,const void *from) {
    const void *fields[]={&file_read_regs.original_nr,&file_read_regs.fd,&file_read_regs.file_command};
    const u32 sizes[]={sizeof(s64),sizeof(s32),sizeof(s32)};
    unsigned index=file_read_calls++;
    if(index>=3 || from!=fields[index] || size!=sizes[index])return -14;
    if(file_read_fail==index+1) {
        memset(to,0xff,size/2);return file_read_error;
    }
    memcpy(to,from,size);return 0;
}
static void original_file_kernel_read_controls(void) {
    unsigned checks=0;
#define READ_CHECK(v) do { assert(v);checks++; } while(0)
    READ_CHECK(sizeof(struct ap_file_entry_snapshot)==16);
    const s32 fds[]={4,0,-1,(-2147483647-1)};
    for(unsigned i=0;i<4;i++) {
        struct ap_task_command owner=file_owner();owner.expected_level=fds[i];
        file_read_regs.original_nr=72;
        file_read_regs.fd=0x7654321000000000ULL|(u32)fds[i];
        file_read_regs.file_command=0xfedcba9800000003ULL;
        file_read_calls=0;file_read_fail=0;
        struct ap_file_entry_snapshot observed={.original_nr=-1,.fd=123,.file_command=456};
        READ_CHECK(ap_file_read_entry(&owner,(u32)owner.expected_level,&file_read_regs.original_nr,
            &file_read_regs.fd,&file_read_regs.file_command,file_entry_read,&observed));
        READ_CHECK(observed.original_nr==72 && observed.fd==fds[i] && observed.file_command==3);
        READ_CHECK(file_read_calls==3);
    }
    /* A failed copy may have partially written its destination. It must not
     * commit any output, proceed to later reads, or make the pre path claim. */
    for(unsigned field=1;field<=3;field++)for(unsigned positive=0;positive<2;positive++) {
        struct ap_task_command owner=file_owner();
        file_read_regs.original_nr=72;file_read_regs.fd=4;file_read_regs.file_command=3;
        file_read_calls=0;file_read_fail=field;file_read_error=positive?1:-14;
        struct ap_file_entry_snapshot observed={.original_nr=-1,.fd=123,.file_command=456},old=observed;
        READ_CHECK(!ap_file_read_entry(&owner,(u32)owner.expected_level,&file_read_regs.original_nr,
            &file_read_regs.fd,&file_read_regs.file_command,file_entry_read,&observed));
        READ_CHECK(!memcmp(&observed,&old,sizeof(old)));
        READ_CHECK(file_read_calls==field);
    }
    for(unsigned bad=0;bad<9;bad++) {
        struct ap_task_command owner=file_owner();u64 lookup_fd=4;
        file_read_regs.original_nr=72;file_read_regs.fd=4;file_read_regs.file_command=3;
        file_read_calls=0;file_read_fail=0;
        if(bad==0)lookup_fd=5;if(bad==1)file_read_regs.original_nr=71;
        if(bad==2)file_read_regs.fd=5;if(bad==3)file_read_regs.file_command=4;
        if(bad==4)owner.operation=AP_ORIGINAL_CONNECT;if(bad==5)owner.generation_before=73;
        if(bad==6)owner.expected_option=4;if(bad==7)lookup_fd|=1ULL<<32;
        if(bad==8)file_read_regs.original_nr|=1LL<<32;
        struct ap_file_entry_snapshot observed={.original_nr=-1,.fd=123,.file_command=456},old=observed;
        READ_CHECK(!ap_file_read_entry(&owner,lookup_fd,&file_read_regs.original_nr,
            &file_read_regs.fd,&file_read_regs.file_command,file_entry_read,&observed));
        READ_CHECK(!memcmp(&observed,&old,sizeof(old)));
        READ_CHECK(file_read_calls==3);
    }
    for(unsigned missing=0;missing<6;missing++) {
        struct ap_task_command owner=file_owner();
        file_read_regs.original_nr=72;file_read_regs.fd=4;file_read_regs.file_command=3;
        file_read_calls=0;file_read_fail=0;
        struct ap_file_entry_snapshot observed={.original_nr=-1,.fd=123,.file_command=456},old=observed;
        READ_CHECK(!ap_file_read_entry(missing==0?NULL:&owner,4,
            missing==1?NULL:&file_read_regs.original_nr,missing==2?NULL:&file_read_regs.fd,
            missing==3?NULL:&file_read_regs.file_command,missing==4?NULL:file_entry_read,
            missing==5?NULL:&observed));
        READ_CHECK(!memcmp(&observed,&old,sizeof(old)));
        READ_CHECK(file_read_calls==0);
    }
    assert(checks==76);printf("original file checked kernel operands: %u checks\n",checks);
#undef READ_CHECK
}

/* The maintained physical-frame adapter reads one exact original return word.
 * Plausible bytes with a failed helper status remain a refusal. */
static u64 file_caller_address,file_caller_word;
static unsigned file_caller_reads;
static long file_caller_error;
static long file_caller_read(void *to,u32 size,const void *from) {
    file_caller_reads++;
    if(size!=sizeof(u64) || (u64)from!=file_caller_address)return -14;
    memcpy(to,&file_caller_word,sizeof(file_caller_word));
    return file_caller_error;
}
static void original_file_caller_controls(void) {
    unsigned checks=0;
#define CALLER_CHECK(v) do {assert(v);checks++;} while(0)
    for(int post=0;post<2;post++) {
        const u64 ip=post?0x107d:0x1006,stack=post?0x5fe8:0x6000;
        const u64 delta=post?0x1e37:0x1eae;
        CALLER_CHECK(ap_file_caller_site(ip,0x2eb4,post));
        CALLER_CHECK(!ap_file_caller_site(0,0x2eb4,post));
        CALLER_CHECK(!ap_file_caller_site(ip,0,post));
        CALLER_CHECK(!ap_file_caller_site(ip,0x2eb5,post));
        CALLER_CHECK(!ap_file_caller_site(ip+1,0x2eb4,post));
        CALLER_CHECK(!ap_file_caller_site(post?0x7d:6,0x1eb4,post));
        CALLER_CHECK(!ap_file_caller_site(~0ULL,delta-1,post));
        CALLER_CHECK(ap_file_caller_site(~0ULL-delta,~0ULL,post));
        CALLER_CHECK(!ap_file_caller_site(~0ULL-delta+1,0,post));
        file_caller_address=0x6000;file_caller_word=0x2eb4;
        file_caller_reads=0;file_caller_error=0;
        CALLER_CHECK(ap_file_lookup_caller(ip,stack,post,file_caller_read)==1);
        CALLER_CHECK(file_caller_reads==1);
        file_caller_word++;file_caller_reads=0;
        CALLER_CHECK(ap_file_lookup_caller(ip,stack,post,file_caller_read)==0);
        CALLER_CHECK(file_caller_reads==1);
        file_caller_word=0x2eb4;
        for(unsigned positive=0;positive<2;positive++) {
            file_caller_reads=0;file_caller_error=positive?1:-14;
            CALLER_CHECK(ap_file_lookup_caller(ip,stack,post,file_caller_read)==-1);
            CALLER_CHECK(file_caller_reads==1);
        }
        file_caller_error=0;
        for(unsigned missing=0;missing<3;missing++) {
            file_caller_reads=0;
            CALLER_CHECK(ap_file_lookup_caller(ip,missing==0?0:missing==1?stack+1:stack,
                post,missing==2?NULL:file_caller_read)==-1);
            CALLER_CHECK(file_caller_reads==0);
        }
    }
    file_caller_reads=0;
    CALLER_CHECK(ap_file_lookup_caller(0x107d,~0ULL-7,1,file_caller_read)==-1);
    CALLER_CHECK(file_caller_reads==0);
    assert(checks==48);printf("original file direct caller/frame: %u checks\n",checks);
#undef CALLER_CHECK
}

/* Production scalar Read boundary helpers with only lower kernel-copy input
 * substitution. These controls do not assert native selection or admission. */
static struct ap_task_command scalar_read_owner(void) {
    return (struct ap_task_command){.provider=3,.command=7,.operation=AP_ORIGINAL_READ,
        .expected_object=11,.generation_before=0x10000000004ULL,.generation_after=17,
        .expected_level=4,.original_count=0x10000000020ULL};
}
static struct ap_fd_call scalar_read_entering(void) {
    struct ap_task_command c=scalar_read_owner();
    return (struct ap_fd_call){.command=7,.file_entry_ip=0x1014,.raw_table=0x2000,
        .operation=AP_ORIGINAL_READ,.original.selection={.command=7,.call=11,.owner_mm=17,
        .provider=3,.task=19,.task_start=23,.table=29,.user_address=c.generation_before,
        .requested_fd=4,.original_count=c.original_count}};
}
static struct {s64 nr;u64 fd,buffer,count;} scalar_regs;
static unsigned scalar_copies,scalar_fail;
static long scalar_copy(void *to,u32 size,const void *from) {
    const void *fields[]={&scalar_regs.nr,&scalar_regs.fd,&scalar_regs.buffer,&scalar_regs.count};
    const u32 sizes[]={8,4,8,8};unsigned i=scalar_copies++;
    if(i>=4 || from!=fields[i] || size!=sizes[i])return -14;
    if(scalar_fail==i+1) {memset(to,0xff,size/2);return -14;}
    memcpy(to,from,size);return 0;
}
static u64 scalar_return_pc;static unsigned scalar_stack_reads;static int scalar_stack_fail;
static long scalar_stack_copy(void *to,u32 size,const void *from) {
    scalar_stack_reads++;
    if(scalar_stack_fail || size!=8 || (u64)from!=0x5ff8)return -14;
    memcpy(to,&scalar_return_pc,8);return 0;
}
static void original_scalar_read_controls(void) {
    unsigned checks=0;
#define SCALAR_CHECK(v) do {assert(v);checks++;} while(0)
    struct ap_task_command owner=scalar_read_owner();
    SCALAR_CHECK(sizeof(struct ap_read_entry_snapshot)==32);
    for(unsigned bad=0;bad<8;bad++) {
        struct ap_task_command o=owner;
        scalar_regs.nr=0;scalar_regs.fd=0xdeadbeef00000004ULL;
        scalar_regs.buffer=o.generation_before;scalar_regs.count=o.original_count;
        if(bad==1)scalar_regs.nr=1;if(bad==2)scalar_regs.nr=1LL<<32;
        if(bad==3)scalar_regs.fd=5;if(bad==4)scalar_regs.buffer^=1ULL<<32;
        if(bad==5)scalar_regs.count^=1ULL<<32;if(bad==6)o.operation=AP_ORIGINAL_FILE;
        if(bad==7)o.expected_option=1;
        struct ap_read_entry_snapshot out={.original_nr=-1,.buffer=3,.count=5,.fd=-7},old=out;
        scalar_copies=scalar_fail=0;
        int good=ap_read_entry_copy(&o,&scalar_regs.nr,&scalar_regs.fd,&scalar_regs.buffer,
            &scalar_regs.count,scalar_copy,&out);
        SCALAR_CHECK(good==(bad==0));SCALAR_CHECK(scalar_copies==4);
        if(!bad)SCALAR_CHECK(out.original_nr==0 && out.fd==4 &&
            out.buffer==owner.generation_before && out.count==owner.original_count);
        else SCALAR_CHECK(!memcmp(&old,&out,sizeof(out)));
    }
    scalar_regs.nr=0;scalar_regs.fd=4;scalar_regs.buffer=owner.generation_before;scalar_regs.count=owner.original_count;
    for(unsigned at=1;at<=4;at++) {
        struct ap_read_entry_snapshot out={.original_nr=-1,.buffer=3,.count=5,.fd=-7},old=out;
        scalar_copies=0;scalar_fail=at;
        SCALAR_CHECK(!ap_read_entry_copy(&owner,&scalar_regs.nr,&scalar_regs.fd,&scalar_regs.buffer,
            &scalar_regs.count,scalar_copy,&out));
        SCALAR_CHECK(scalar_copies==at);SCALAR_CHECK(!memcmp(&old,&out,sizeof(out)));
    }
    scalar_fail=0;
    SCALAR_CHECK(ap_read_fentry_session_marker(
        0xffffffff81fae890ULL,0xffffffff81fae8a4ULL));
    SCALAR_CHECK(!ap_read_fentry_session_marker(
        0xffffffff81fae8a4ULL,0xffffffff81fae8a4ULL));
    SCALAR_CHECK(!ap_read_fentry_session_marker(0,0xffffffff81fae8a4ULL));
    SCALAR_CHECK(ap_read_fentry_fdget_site(
        0xffffffff81fae890ULL,0xffffffff81fae8b5ULL));
    SCALAR_CHECK(!ap_read_fentry_fdget_site(
        0xffffffff81fae890ULL,0xffffffff81fae8b6ULL));
    SCALAR_CHECK(!ap_read_fentry_fdget_site(~0ULL,0x24));
    const u64 session_words[]={0,0x8001};
    for(unsigned i=0;i<2;i++) {
        struct ap_fd_call c=scalar_read_entering();
        SCALAR_CHECK(ap_read_selection_enter(&c,&owner,19,23,0x2000,29,14,0x6000));
        SCALAR_CHECK(ap_read_selection_session_post(
            &c,&owner,19,23,0x2000,29,session_words[i]));
        SCALAR_CHECK(ap_fd_selection_complete(&c.selection) &&
            c.selection.word==session_words[i] && c.entry_stack==0x6000);
        struct ap_fd_call before=c;
        SCALAR_CHECK(!ap_read_selection_session_post(
            &c,&owner,19,23,0x2000,29,session_words[i]) && !memcmp(&c,&before,sizeof(c)));
    }
    {
        struct ap_fd_call c=scalar_read_entering();
        SCALAR_CHECK(ap_read_selection_enter(&c,&owner,19,23,0x2000,29,14,0x6000));
        struct ap_fd_call before=c;
        SCALAR_CHECK(!ap_read_selection_session_post(&c,&owner,19,23,0x2000,30,0x8001));
        SCALAR_CHECK(!memcmp(&c,&before,sizeof(c)));
    }
    const u64 words[]={0,0,0x8000,0x8001},cookies[]={15,16,15,15};
    for(unsigned i=0;i<4;i++) {
        struct ap_fd_call c=scalar_read_entering(),old=c;
        SCALAR_CHECK(!ap_read_selection_post(&c,&owner,19,23,0x2000,29,cookies[i],
            owner.generation_before,owner.original_count,words[i]));
        SCALAR_CHECK(!memcmp(&old,&c,sizeof(c)));
        SCALAR_CHECK(ap_read_selection_enter(&c,&owner,19,23,0x2000,29,14,0x6000));
        SCALAR_CHECK(!ap_read_selection_enter(&c,&owner,19,23,0x2000,29,14,0x6000));
        scalar_return_pc=0x1025;scalar_stack_reads=0;scalar_stack_fail=0;
        SCALAR_CHECK(ap_read_direct_call(&c,0x5fe0,scalar_stack_copy)==1 && scalar_stack_reads==1);
        SCALAR_CHECK(ap_read_selection_post(&c,&owner,19,23,0x2000,29,cookies[i],
            owner.generation_before,owner.original_count,words[i]));
        SCALAR_CHECK(ap_fd_selection_complete(&c.selection) && c.selection.word==words[i] && c.entry_stack==0x6000);
        old=c;
        SCALAR_CHECK(!ap_read_selection_post(&c,&owner,19,23,0x2000,29,cookies[i],
            owner.generation_before,owner.original_count,words[i]) && !memcmp(&old,&c,sizeof(c)));
        /* Same retained direct caller remains recognizable after ready, so a
         * duplicate fails; unrelated nesting is not interpreted as an owner. */
        c.original.selection.ready=1;scalar_stack_reads=0;
        SCALAR_CHECK(ap_read_direct_call(&c,0x5fd0,scalar_stack_copy)==0 && !scalar_stack_reads);
        scalar_return_pc=0x1035;
        SCALAR_CHECK(ap_read_direct_call(&c,0x5fe0,scalar_stack_copy)==0 && scalar_stack_reads==1);
        scalar_stack_fail=1;
        SCALAR_CHECK(ap_read_direct_call(&c,0x5fe0,scalar_stack_copy)==-1);
        scalar_stack_fail=0;
    }
    for(unsigned bad=0;bad<16;bad++) {
        struct ap_task_command o=owner;struct ap_fd_call c=scalar_read_entering();
        SCALAR_CHECK(ap_read_selection_enter(&c,&o,19,23,0x2000,29,14,0x6000));
        u64 task=19,start=23,raw=0x2000,table=29,cookie=15,buffer=o.generation_before,count=o.original_count,word=0x8001;
        if(bad==0)o.provider++;if(bad==1)o.command++;if(bad==2)o.expected_object++;
        if(bad==3)o.generation_after++;if(bad==4)o.expected_level++;if(bad==5)o.expected_option++;
        if(bad==6)task++;if(bad==7)start++;if(bad==8)raw++;if(bad==9)table++;
        if(bad==10)cookie=13;if(bad==11)buffer^=1ULL<<32;if(bad==12)count^=1ULL<<32;
        if(bad==13)word=0x8002;if(bad==14)word=1;if(bad==15)cookie=16;
        struct ap_fd_call old=c;
        SCALAR_CHECK(!ap_read_selection_post(&c,&o,task,start,raw,table,cookie,buffer,count,word));
        SCALAR_CHECK(!memcmp(&old,&c,sizeof(c)));
    }
    struct ap_fd_call c=scalar_read_entering();c.entry_stack=0x6000;
    c.selection=(struct ap_fd_selection){1,1,0x8001};c.selected_file=31;
    c.original.selection.file=31;c.original.selection.fdput_flags=1;c.original.selection.ready=1;
    struct ap_command_result r={.command=7,.operation=AP_ORIGINAL_READ,.task=19,.start_boottime=23,
        .identity={.provider=3},.phase=AP_COMMAND_RUNNING,.original_count=owner.original_count};
    /* Raw restart errors are retained, not normalized to a backend's private
     * synthetic interruption; the latter has no native Returned witness. */
    const s64 values[]={-4095,-512,-513,-514,-516,-9,-4,0,7,0x7ffff000};
    for(unsigned i=0;i<10;i++) {
        SCALAR_CHECK(ap_original_read_native_return(&owner,&r,&c,19,23,0,4,
            owner.generation_before,owner.original_count,values[i]));
        struct ap_original_result result=c.original;result.complete=1;result.returned=(s32)values[i];
        struct ap_command_result done=r;done.phase=AP_COMMAND_DONE;done.returned=(s32)values[i];
        SCALAR_CHECK(ap_original_result_matches(&owner,&done,&result));
    }
    SCALAR_CHECK(!ap_original_read_return_value(owner.original_count,0x7ffff001));
    SCALAR_CHECK(!ap_original_read_return_value(owner.original_count,-4096));
    SCALAR_CHECK(ap_original_read_return_value(0,0));SCALAR_CHECK(!ap_original_read_return_value(0,1));
    SCALAR_CHECK(ap_original_read_return_value(7,7));SCALAR_CHECK(!ap_original_read_return_value(7,8));
    for(unsigned bad=0;bad<16;bad++) {
        struct ap_task_command o=owner;struct ap_command_result q=r;struct ap_fd_call b=c;
        u64 task=19,start=23,buffer=o.generation_before,count=o.original_count;s64 nr=0;s32 fd=4;
        if(bad==0)task++;if(bad==1)start++;if(bad==2)nr=1;if(bad==3)fd++;
        if(bad==4)buffer^=1ULL<<32;if(bad==5)count^=1ULL<<32;if(bad==6)q.command++;
        if(bad==7)q.phase=AP_COMMAND_DONE;if(bad==8)q.original_count++;if(bad==9)b.selected_file++;
        if(bad==10)b.original.selection.ready=0;if(bad==11)b.entry_stack=0;
        if(bad==12)b.original.complete=1;if(bad==13)b.original.problem=AP_FD_MISSING;
        if(bad==14)b.selection.word=0;if(bad==15)o.original_count++;
        SCALAR_CHECK(!ap_original_read_native_return(&o,&q,&b,task,start,nr,fd,buffer,count,0));
    }
    c.selected_file=0;c.selection.word=0;c.original.selection.file=0;c.original.selection.fdput_flags=0;
    SCALAR_CHECK(ap_original_read_native_return(&owner,&r,&c,19,23,0,4,owner.generation_before,owner.original_count,-9));
    SCALAR_CHECK(!ap_original_read_native_return(&owner,&r,&c,19,23,0,4,owner.generation_before,owner.original_count,0));
    c.original.selection.ready=0;
    SCALAR_CHECK(!ap_original_read_native_return(&owner,&r,&c,19,23,0,4,owner.generation_before,owner.original_count,-9));
    c.original.selection.ready=1;c.original.complete=1;c.original.returned=-9;r.phase=AP_COMMAND_DONE;r.returned=-9;
    SCALAR_CHECK(ap_original_result_matches(&owner,&r,&c.original));
    for(unsigned bad=0;bad<14;bad++) {
        struct ap_original_result result=c.original;struct ap_command_result q=r;
        if(bad==0)result.copy_entered=1;if(bad==1)result.copy_returned=1;if(bad==2)result.copy_remaining=1;
        if(bad==3)result.audit_entered=1;if(bad==4)result.audit_returned=1;if(bad==5)result.audit_result=-1;
        if(bad==6)result.security_entered=1;if(bad==7)result.security_returned=1;if(bad==8)result.security_result=-1;
        if(bad==9)result.address[0]=1;if(bad==10)q.original_count++;if(bad==11)result.selection.original_count++;
        if(bad==12)q.state.lowat=1;if(bad==13)q.returned=0;
        SCALAR_CHECK(!ap_original_result_matches(&owner,&q,&result));
    }
    const char *symbols[]={"__x64_sys_read","fdget_pos","fdget_pos"};
    const u64 offsets[]={0x13,0x96,0xfa};
    for(unsigned i=0;i<3;i++) {
        struct bpf_prog_info p={.type=BPF_PROG_TYPE_KPROBE,.id=17};
        struct bpf_link_info l={.type=BPF_LINK_TYPE_PERF_EVENT,.id=23,.prog_id=17};
        l.perf_event.type=BPF_PERF_EVENT_KPROBE;l.perf_event.kprobe.name_len=(u32)strlen(symbols[i])+1;
        l.perf_event.kprobe.offset=offsets[i];l.perf_event.kprobe.cookie=14+i;
        SCALAR_CHECK(ap_fdget_link_matches(4+i,&p,sizeof(p),&l,sizeof(l),symbols[i]));
        const char wrong[64]="ksys_read";
        for(unsigned bad=0;bad<7;bad++) {
            struct bpf_prog_info bp=p;struct bpf_link_info bl=l;
            if(bad==0)bp.recursion_misses=1;if(bad==1)bl.perf_event.kprobe.missed=1;
            if(bad==2)bl.perf_event.kprobe.offset++;if(bad==3)bl.perf_event.kprobe.cookie++;
            if(bad==4)bl.prog_id++;if(bad==5)bl.perf_event.kprobe.name_len--;
            SCALAR_CHECK(!ap_fdget_link_matches(4+i,&bp,sizeof(bp),&bl,sizeof(bl),bad==6?wrong:symbols[i]));
        }
    }
    /* The additive operand cannot change the meaning of old operations. */
    struct ap_task_command old=file_owner();struct ap_fd_call oldcall=file_entering();
    old.original_count=1;
    SCALAR_CHECK(!ap_file_entry_operands(&old,4,72,4,3));
    SCALAR_CHECK(!ap_file_selection_enter(&oldcall,&old,19,23,0x2000,29,12,0x6000));
    for(unsigned op=0;op<2;op++) {
        struct ap_task_command o=owner;o.operation=op?AP_ORIGINAL_CLOSE:AP_ORIGINAL_CONNECT;
        o.generation_before=0;o.original_count=0;
        struct ap_original_selection selected=scalar_read_entering().original.selection;
        selected.user_address=0;selected.original_count=0;selected.ready=1;
        SCALAR_CHECK(ap_original_selection_matches(&o,&selected));
        o.original_count=1;SCALAR_CHECK(!ap_original_selection_matches(&o,&selected));
        o.original_count=0;selected.original_count=1;
        SCALAR_CHECK(!ap_original_selection_matches(&o,&selected));
    }
    assert(checks==238);
    printf("scalar original Read production controls=%u (native execution not implied)\n",checks);
#undef SCALAR_CHECK
}

/* Actual production Socket predicate controls. No kernel effect is simulated
 * as native evidence; these exercise the exact exported result validators. */
static unsigned socket_reads, socket_fail_read;
static long socket_checked_read(void *dst,u32 size,const void *src) {
    socket_reads++;
    if(socket_reads==socket_fail_read)return -14;
    memcpy(dst,src,size);return 0;
}
static void original_socket_controls(void) {
    unsigned checks=0;
#define SOCKET_CHECK(v) do {assert(v);checks++;} while(0)
    struct ap_task_command c={.provider=3,.command=7,.operation=AP_ORIGINAL_SOCKET_CALL,
        .expected_object=11,.generation_before=0x80001,.generation_after=17,
        .expected_level=2,.expected_option=6};
    struct ap_original_result o={.selection={.command=7,.call=11,.owner_mm=17,
        .provider=3,.task=19,.task_start=23,.table=29,.file=31,.user_address=0x80001,
        .ready=1,.requested_fd=2,.address_length=6},.installation={.begin=41,.end=42,.fd=8},
        .returned=8,.complete=1};
    struct ap_command_result r={.command=7,.operation=AP_ORIGINAL_SOCKET_CALL,.task=19,
        .start_boottime=23,.identity={.provider=3},.returned=8,.phase=AP_COMMAND_DONE};
    SOCKET_CHECK(AP_ORIGINAL_SOCKET==5 && AP_ORIGINAL_SOCKET_CALL==12);
    SOCKET_CHECK(ap_original_operation(AP_ORIGINAL_SOCKET_CALL));
    SOCKET_CHECK(ap_original_result_matches(&c,&r,&o));
    SOCKET_CHECK(ap_original_socket_operands(&c,41,2,0x80001,6));
    SOCKET_CHECK(!ap_original_socket_operands(&c,42,2,0x80001,6));
    SOCKET_CHECK(!ap_original_socket_operands(&c,41,3,0x80001,6));
    SOCKET_CHECK(!ap_original_socket_operands(&c,41,2,1,6));
    SOCKET_CHECK(!ap_original_socket_operands(&c,41,2,0x80001,0));
#define SOCKET_BAD(field,value) do {struct ap_original_result b=o;b.field=(value);SOCKET_CHECK(!ap_original_result_matches(&c,&r,&b));}while(0)
    SOCKET_BAD(selection.command,8);SOCKET_BAD(selection.call,12);SOCKET_BAD(selection.owner_mm,18);
    SOCKET_BAD(selection.provider,4);SOCKET_BAD(selection.task,20);SOCKET_BAD(selection.task_start,24);
    SOCKET_BAD(selection.table,0);SOCKET_BAD(selection.file,0);SOCKET_BAD(selection.requested_fd,3);
    SOCKET_BAD(selection.user_address,1);SOCKET_BAD(selection.address_length,0);
    SOCKET_BAD(selection.original_count,1);SOCKET_BAD(selection.ready,0);SOCKET_BAD(selection.fdput_flags,1);
    SOCKET_BAD(installation.begin,0);SOCKET_BAD(installation.end,41);SOCKET_BAD(installation.fd,9);
    SOCKET_BAD(installation.reserved,1);SOCKET_BAD(complete,0);SOCKET_BAD(problem,1);SOCKET_BAD(reserved,1);
    SOCKET_BAD(copy_entered,1);SOCKET_BAD(copy_returned,1);SOCKET_BAD(copy_remaining,1);
    SOCKET_BAD(audit_entered,1);SOCKET_BAD(audit_returned,1);SOCKET_BAD(audit_result,-1);
    SOCKET_BAD(security_entered,1);SOCKET_BAD(security_returned,1);SOCKET_BAD(security_result,-1);
    SOCKET_BAD(address[127],1);SOCKET_BAD(returned,9);
#undef SOCKET_BAD
    struct ap_original_result absent=o;struct ap_command_result error=r;
    absent.selection.file=0;memset(absent.address,0,sizeof(absent.address));
    for(int errno_value=1;errno_value<=4095;errno_value+=4094) {
        absent.returned=error.returned=-errno_value;
        SOCKET_CHECK(ap_original_result_matches(&c,&error,&absent));
        struct ap_original_result partial=absent;partial.selection.ready=0;
        SOCKET_CHECK(!ap_original_result_matches(&c,&error,&partial));
        partial=absent;partial.selection.file=31;
        SOCKET_CHECK(!ap_original_result_matches(&c,&error,&partial));
        partial=absent;partial.installation.begin=41;
        SOCKET_CHECK(!ap_original_result_matches(&c,&error,&partial));
    }
    absent.returned=error.returned=-4096;SOCKET_CHECK(!ap_original_result_matches(&c,&error,&absent));
    absent.returned=error.returned=0;SOCKET_CHECK(!ap_original_result_matches(&c,&error,&absent));
    struct ap_socket_entry_snapshot regs={.nr=41,.domain=2,.type=0x80001,.protocol=6},out;
    memset(&out,0xa5,sizeof(out));struct ap_socket_entry_snapshot untouched=out;
    for(unsigned fault=1;fault<=4;fault++) {
        socket_reads=0;socket_fail_read=fault;
        SOCKET_CHECK(!ap_socket_entry_copy(&c,&regs.nr,&regs.domain,&regs.type,&regs.protocol,socket_checked_read,&out));
        SOCKET_CHECK(!memcmp(&out,&untouched,sizeof(out)));
        SOCKET_CHECK(socket_reads==fault);
    }
    socket_fail_read=0;socket_reads=0;
    SOCKET_CHECK(ap_socket_entry_copy(&c,&regs.nr,&regs.domain,&regs.type,&regs.protocol,socket_checked_read,&out));
    SOCKET_CHECK(socket_reads==4 && !memcmp(&out,&regs,sizeof(out)));
    struct ap_fd_call call={.command=7,.operation=AP_ORIGINAL_SOCKET_CALL,.install_begin=41,.new_file=0x1000,.original=o};
    call.original.complete=0;struct ap_command_result running=r;running.phase=AP_COMMAND_RUNNING;
    SOCKET_CHECK(ap_original_socket_native_return(&c,&running,&call,19,23,41,2,0x80001,6,8));
    SOCKET_CHECK(!ap_original_socket_native_return(&c,&running,&call,19,23,41,2,0x80001,6,-12));
    SOCKET_CHECK(!ap_original_socket_native_return(&c,&running,&call,19,23,41,2,0x80001,6,9));
    SOCKET_CHECK(!ap_original_socket_native_return(&c,&running,&call,19,23,41,2,0x80001,6,0x80000000LL));
    SOCKET_CHECK(!ap_original_socket_native_return(&c,&running,&call,20,23,41,2,0x80001,6,8));
    call.original=absent;call.original.complete=0;call.original.returned=0;
    call.install_begin=call.new_file=0;
    SOCKET_CHECK(ap_original_socket_native_return(&c,&running,&call,19,23,41,2,0x80001,6,-12));
    SOCKET_CHECK(!ap_original_socket_native_return(&c,&running,&call,19,23,41,2,0x80001,6,8));
    SOCKET_CHECK(!ap_original_socket_native_return(&c,&running,&call,19,23,42,2,0x80001,6,-12));
    assert(checks==72);
    printf("original Socket production controls=%u (native execution not implied)\n",checks);
#undef SOCKET_CHECK
}

/* Actual allocator issuer/result predicates. This is not a native fd_install
 * or FIFO scheduling proof; those remain required component/native controls. */
/* Both creator ABIs retain Linux-owned error ordering. This checks receipt
 * predicates only; real syscall/BPF installation evidence remains required. */
static void original_epoll_allocator_controls(void) {
    for(unsigned legacy=0;legacy<2;legacy++) {
        u64 nr=legacy?AP_EPOLL_CREATE_SYSCALL:AP_EPOLL_CREATE1_SYSCALL;
        int argument=legacy?1:AP_EPOLL_CLOEXEC;
        struct ap_task_command c={.provider=3,.command=7,.operation=AP_ORIGINAL_EPOLL_CALL,
            .expected_object=11,.generation_before=nr,.generation_after=17,.expected_level=argument};
        struct ap_allocator_entry_snapshot regs={.nr=nr,.arg0=0xfeedface00000000ULL|(u32)argument,
            .arg1=~0ULL,.arg2=0xf0123456abcdef98ULL,.arg3=0xfeed1234},out={0};
        assert(ap_original_allocator(19) && ap_original_operation(19));
        assert(ap_original_allocator_operands(&c,nr,regs.arg0,regs.arg1,regs.arg2,regs.arg3));
        assert(!ap_original_allocator_operands(&c,legacy?291:213,regs.arg0,regs.arg1,regs.arg2,regs.arg3));
        assert(!ap_original_allocator_operands(&c,nr,regs.arg0+1,regs.arg1,regs.arg2,regs.arg3));
        socket_reads=socket_fail_read=0;
        assert(ap_allocator_entry_copy(&c,&regs.nr,&regs.arg0,&regs.arg1,&regs.arg2,&regs.arg3,socket_checked_read,&out));
        assert(out.nr==nr && out.arg0==regs.arg0 && socket_reads==2 && !out.arg1 && !out.arg2 && !out.arg3);
        for(unsigned cloexec=0;cloexec<2;cloexec++) {
            struct ap_original_result o={.selection={.command=7,.call=11,.owner_mm=17,.provider=3,
                .task=19,.task_start=23,.table=29,.file=31,.user_address=nr,.ready=1,.requested_fd=argument},
                .installation={.begin=41,.end=42,.fd=8},.returned=8,.complete=1};
            o.epoll.status_flags=2;o.epoll.descriptor_flags=cloexec;o.epoll.profiled=1;
            struct ap_command_result r={.command=7,.operation=AP_ORIGINAL_EPOLL_CALL,.task=19,
                .start_boottime=23,.identity={.provider=3},.returned=8,.phase=AP_COMMAND_DONE};
            assert(ap_original_result_matches(&c,&r,&o));
            for(unsigned bad=0;bad<10;bad++) {
                struct ap_original_result changed=o;
                switch(bad) {
                case 0:changed.selection.file=0;break;
                case 1:changed.selection.user_address=legacy?291:213;break;
                case 2:changed.selection.task_start++;break;
                case 3:changed.epoll.status_flags|=AP_EPOLL_CLOEXEC;break;
                case 4:changed.epoll.descriptor_flags=2;break;
                case 5:changed.epoll.profiled=0;break;
                case 6:changed.address[127]=1;break;
                case 7:changed.installation.end=41;break;
                case 8:changed.complete=0;break;
                case 9:changed.returned=-22;break;
                }
                assert(!ap_original_result_matches(&c,&r,&changed));
            }
            // Bad original argument is admitted, but only its positive actual
            // error completion with no installation can certify an error.
            c.expected_level=legacy?0:1;o.selection.requested_fd=c.expected_level;
            assert(ap_original_allocator_operands(&c,nr,(u32)c.expected_level,~0ULL,~0ULL,~0ULL));
            assert(!ap_original_result_matches(&c,&r,&o));
            memset(o.address,0,sizeof(o.address));o.selection.file=0;o.returned=r.returned=-22;
            assert(ap_original_result_matches(&c,&r,&o));
            o.complete=0;assert(!ap_original_result_matches(&c,&r,&o));
            c.expected_level=argument;
        }
    }
    puts("original epoll allocator: both native operand shapes and exact positive/negative receipt controls");
}

static void original_openat_controls(void) {
    struct ap_task_command c={.provider=3,.command=7,.operation=AP_ORIGINAL_OPENAT_CALL,
        .expected_object=11,.generation_before=0x7ff012345678ULL,.generation_after=17,
        .expected_level=-100,.expected_option=0x80042,.original_count=0xfeedface000001a4ULL};
    struct ap_allocator_entry_snapshot regs={.nr=257,.arg0=0x12345678ffffff9cULL,
        .arg1=c.generation_before,.arg2=0x9876543200080042ULL,.arg3=c.original_count},out;
    assert(AP_ORIGINAL_OPENAT_CALL==18 && ap_original_operation(18));
    assert(ap_original_allocator_operands(&c,regs.nr,regs.arg0,regs.arg1,regs.arg2,regs.arg3));
    assert(!ap_original_allocator_operands(&c,41,regs.arg0,regs.arg1,regs.arg2,regs.arg3));
    assert(!ap_original_allocator_operands(&c,257,regs.arg0+1,regs.arg1,regs.arg2,regs.arg3));
    assert(!ap_original_allocator_operands(&c,257,regs.arg0,regs.arg1+1,regs.arg2,regs.arg3));
    assert(!ap_original_allocator_operands(&c,257,regs.arg0,regs.arg1,regs.arg2+1,regs.arg3));
    assert(!ap_original_allocator_operands(&c,257,regs.arg0,regs.arg1,regs.arg2,regs.arg3+1));
    // Socket retains its original typed interpretation and ignores arg3.
    struct ap_task_command socket={.operation=AP_ORIGINAL_SOCKET_CALL,
        .expected_level=2,.generation_before=1,.expected_option=6};
    assert(ap_original_allocator_operands(&socket,41,2,1,6,~0ULL));
    memset(&out,0xa5,sizeof(out));struct ap_allocator_entry_snapshot untouched=out;
    for(unsigned fault=1;fault<=5;fault++) {
        socket_reads=0;socket_fail_read=fault;
        assert(!ap_allocator_entry_copy(&c,&regs.nr,&regs.arg0,&regs.arg1,&regs.arg2,
            &regs.arg3,socket_checked_read,&out));
        assert(!memcmp(&out,&untouched,sizeof(out)) && socket_reads==fault);
    }
    socket_reads=socket_fail_read=0;
    assert(ap_allocator_entry_copy(&c,&regs.nr,&regs.arg0,&regs.arg1,&regs.arg2,
        &regs.arg3,socket_checked_read,&out));
    assert(socket_reads==5 && !memcmp(&out,&regs,sizeof(out)));
    struct ap_original_result o={.selection={.command=7,.call=11,.owner_mm=17,
        .provider=3,.task=19,.task_start=23,.table=29,.file=31,.user_address=c.generation_before,
        .ready=1,.requested_fd=-100,.address_length=0x80042,.original_count=c.original_count},
        .installation={.begin=41,.end=42,.fd=8},.returned=8,.complete=1};
    o.opened.mode=0100644;o.opened.status_flags=0x8002;
    o.opened.device_major=0;o.opened.device_minor=0;
    struct ap_command_result r={.command=7,.operation=AP_ORIGINAL_OPENAT_CALL,.task=19,
        .start_boottime=23,.identity={.provider=3},.returned=8,.phase=AP_COMMAND_DONE,
        .original_count=c.original_count};
    assert(ap_original_result_matches(&c,&r,&o));
    struct ap_original_result no_profile=o;no_profile.opened.mode=0;
    assert(!ap_original_result_matches(&c,&r,&no_profile));
    no_profile=o;no_profile.opened.mode=0x10000;
    assert(!ap_original_result_matches(&c,&r,&no_profile));
    struct ap_original_result wrong=o;wrong.selection.file=0;
    assert(!ap_original_result_matches(&c,&r,&wrong));
    wrong=o;wrong.selection.original_count^=1;
    assert(!ap_original_result_matches(&c,&r,&wrong));
    wrong=o;wrong.installation.end=wrong.installation.begin;
    assert(!ap_original_result_matches(&c,&r,&wrong));
    wrong=o;wrong.installation.fd=9;
    assert(!ap_original_result_matches(&c,&r,&wrong));
    wrong=o;wrong.address[127]=1;
    assert(!ap_original_result_matches(&c,&r,&wrong));
    struct ap_command_result bad=r;bad.original_count^=1;
    assert(!ap_original_result_matches(&c,&bad,&o));
    struct ap_fd_call call={.command=7,.operation=AP_ORIGINAL_OPENAT_CALL,.install_begin=41,
        .new_file=0x1000,.original=o};call.original.complete=0;
    struct ap_command_result running=r;running.phase=AP_COMMAND_RUNNING;
    assert(ap_original_allocator_native_return(&c,&running,&call,19,23,257,
        regs.arg0,regs.arg1,regs.arg2,regs.arg3,8));
    assert(!ap_original_allocator_native_return(&c,&running,&call,19,23,257,
        regs.arg0,regs.arg1,regs.arg2,regs.arg3,-12));
    assert(!ap_original_allocator_native_return(&c,&running,&call,19,23,257,
        regs.arg0,regs.arg1,regs.arg2,regs.arg3,9));
    assert(!ap_original_allocator_native_return(&c,&running,&call,20,23,257,
        regs.arg0,regs.arg1,regs.arg2,regs.arg3,8));
    o.selection.file=0;memset(o.address,0,sizeof(o.address));
    for(int error=1;error<=4095;error+=4094) {
        o.returned=r.returned=-error;
        assert(ap_original_result_matches(&c,&r,&o));
    }
    o.returned=r.returned=0;
    assert(!ap_original_result_matches(&c,&r,&o)); // no invented installation on seccomp ERRNO(0)
    o.returned=r.returned=-4096;
    assert(!ap_original_result_matches(&c,&r,&o));
    call.original=o;call.original.complete=0;call.install_begin=call.new_file=0;
    assert(ap_original_allocator_native_return(&c,&running,&call,19,23,257,
        regs.arg0,regs.arg1,regs.arg2,regs.arg3,-12));
    assert(!ap_original_allocator_native_return(&c,&running,&call,19,23,257,
        regs.arg0,regs.arg1,regs.arg2,regs.arg3,0));
    puts("original Openat allocator controls (native execution not implied)");
}

/* These use the actual issuer/collector predicates; no kernel copy is
 * simulated as evidence. Actual wrapper, UFFD and session qualification remain
 * separately required for the image-bound errno interpretation. */
static void epoll_ctl_copy_controls(void) {
    unsigned checks=0;
#define EC_PRED(v) do {assert(v);checks++;} while(0)
    const int ops[]={1,2,3,0,-1,0x7fffffff};
    for(unsigned n=0;n<sizeof(ops)/sizeof(ops[0]);n++) {
        struct ap_task_command c={.provider=3,.command=7,.operation=AP_EPOLL_CTL_COPY,
            .expected_object=11,.generation_before=0x10000000004ULL,.generation_after=17,
            .expected_level=ops[n],.expected_option=233};
        EC_PRED(ap_epoll_ctl_operands(&c,233,0x33,~0ULL,(u64)(s64)ops[n],~0ULL,c.generation_before));
        EC_PRED(!ap_epoll_ctl_operands(&c,233,0x23,~0ULL,(u64)(s64)ops[n],~0ULL,c.generation_before));
        EC_PRED(!ap_epoll_ctl_operands(&c,232,0x33,~0ULL,(u64)(s64)ops[n],~0ULL,c.generation_before));
        EC_PRED(!ap_epoll_ctl_operands(&c,233,0x33,4,(u64)(s64)ops[n],~0ULL,c.generation_before));
        EC_PRED(!ap_epoll_ctl_operands(&c,233,0x33,~0ULL,(u64)(s64)ops[n],4,c.generation_before));
        EC_PRED(!ap_epoll_ctl_operands(&c,233,0x33,~0ULL,(u64)(s64)ops[n],~0ULL,c.generation_before^(1ULL<<32)));
        struct ap_epoll_ctl_copy copy={.command=7,.call=11,.owner_mm=17,.provider=3,.task=19,.task_start=23,
            .user_address=c.generation_before,.table=29,.entered=1,.op=ops[n],.image_wakeup_policy=AP_EPOLL_IMAGE_PM_SLEEP_DISABLED};
        EC_PRED(!ap_epoll_ctl_outcome(&copy,-9)); /* native entry alone is not copied bytes */
        EC_PRED(ap_epoll_ctl_outcome(&copy,-14)==(ops[n]!=2));
        struct ap_epoll_ctl_copy absent=copy;absent.entered=0;
        EC_PRED(!ap_epoll_ctl_outcome(&absent,-14)); /* errno plus absence is never proof */
        copy.ctl_entered=1;EC_PRED(!ap_epoll_ctl_outcome(&copy,-9));
        copy.ctl_returned=1;EC_PRED(ap_epoll_ctl_outcome(&copy,-9));
        EC_PRED(!ap_epoll_ctl_outcome(&copy,-14));
        for(unsigned b=0;b<12;b++)copy.event[b]=(u8)(0x80+b);
        EC_PRED(ap_epoll_ctl_outcome(&copy,-9)==(ops[n]!=2));
        if(ops[n]==2)memset(copy.event,0,sizeof(copy.event));
        copy.complete=1;copy.returned=-9;
        struct ap_command_result r={.command=7,.operation=AP_EPOLL_CTL_COPY,.task=19,.start_boottime=23,
            .identity.provider=3,.returned=-9,.phase=AP_COMMAND_DONE};
        EC_PRED(ap_epoll_ctl_copy_matches(&c,&r,&copy));
        EC_PRED(!ap_epoll_ctl_outcome(&copy,-9)); /* duplicate publication refuses */
        c.original_count=1ULL<<32;EC_PRED(!ap_epoll_ctl_copy_matches(&c,&r,&copy));
        c.original_count=0;copy.task_start++;EC_PRED(!ap_epoll_ctl_copy_matches(&c,&r,&copy));
    }
    EC_PRED(ap_epoll_ctl_caller_site(0xffffffff81fb1ca0ULL,0xffffffff81facabcULL));
    EC_PRED(!ap_epoll_ctl_caller_site(0xffffffff81fb1ca0ULL,0xffffffff81facaa1ULL));
    EC_PRED(!ap_epoll_ctl_caller_site(0,0));
    printf("epoll copy exact native operands/receipt predicates: %u checks\n",checks);
#undef EC_PRED
}

/* Execute the actual BPF callback bodies with controlled kernel-helper
 * boundaries. This is host control coverage, not a native probe receipt. */
#include <stdbool.h>
struct ecs_regs { u64 orig_ax,cs,di,si,dx,r10,sp,cx,r8,ax,ip,bp,bx,r13,r12; };
struct ecs_task { void *files,*mm; };
static struct {
    struct ap_task_command owner;
    struct ap_command_result result;
    struct ap_fd_call call;
    struct ecs_task task;
    struct ecs_regs user,ctl;
    struct { u64 caller;u8 event[12]; } stack;
    u64 cookie,ip,problem,attach_cookie;
    struct ap_fd_status status;
    unsigned reads,published,read_fault;
    bool present,returning;
} ecs;
static int ecs_map;
static struct ap_task_command *ecs_command(void) {return &ecs.owner;}
static struct ap_invocation_key ecs_actor(void) {
    return (struct ap_invocation_key){.task=19,.start=23};
}
static struct ecs_task *ecs_current_task(void) {return &ecs.task;}
static u64 ecs_table(void *files,int create) {
    assert(create==1);return files==ecs.task.files && files?29:0;
}
static struct ap_command_result *ecs_claim(const struct ap_task_command *c) {
    if(!ap_command_reservation_matches(c,&ecs.result,ap_command_slot(c->command)))return NULL;
    ecs.result.phase=AP_COMMAND_RUNNING;ecs.result.task=19;ecs.result.start_boottime=23;
    return &ecs.result;
}
static struct ap_command_result *ecs_result(u64 ticket) {
    return ticket==ecs.result.command?&ecs.result:NULL;
}
static int ecs_update(void *map,const struct ap_invocation_key *key,
                      const struct ap_fd_call *value,u64 flags) {
    assert(map==&ecs_map && key->task==19 && key->start==23 && flags==BPF_NOEXIST);
    if(ecs.present)return -17;
    ecs.call=*value;ecs.present=true;return 0;
}
static struct ap_fd_call *ecs_lookup(void *map,const struct ap_invocation_key *key) {
    assert(map==&ecs_map && key->task==19 && key->start==23);
    return ecs.present?&ecs.call:NULL;
}
static void ecs_problem(u64 problem) {ecs.problem|=problem;}
/* The actual op20 callbacks share this host fixture's helper boundary. These
 * identities are explicit fixture values, never native qualification. */
struct file;
static struct ap_fd_status *ecs_stats(void) {return &ecs.status;}
static u64 ecs_incarnation(void) {return ecs.owner.provider;}
static u64 ecs_attach_cookie(void *ctx) {assert(ctx==&ecs.ctl);return ecs.attach_cookie;}
static u64 ecs_file(struct file *file) {
    if((uintptr_t)file==0x8000)return 31;
    if((uintptr_t)file==0x9000)return 37;
    return 0;
}

static bool ecs_is_return(void *ctx) {assert(ctx==&ecs.ctl);return ecs.returning;}
static u64 *ecs_cookie(void *ctx) {assert(ctx==&ecs.ctl);return &ecs.cookie;}
static u64 ecs_ip(void *ctx) {assert(ctx==&ecs.ctl);return ecs.ip;}
static long ecs_read(void *to,u32 size,const void *from) {
    ecs.reads++;
    if(from==&ecs.stack.caller && size==sizeof(ecs.stack.caller)) {
        if(ecs.read_fault==1)return -14;
    } else if(from==ecs.stack.event && size==sizeof(ecs.stack.event)) {
        if(ecs.read_fault==2)return -14;
    } else return -14;
    memcpy(to,from,size);return 0;
}
static void ecs_publish(struct ap_command_result *r) {
    assert(r==&ecs.result && r->phase==AP_COMMAND_RUNNING);
    r->phase=AP_COMMAND_DONE;ecs.published++;
}
#define pt_regs ecs_regs
#define task_struct ecs_task
#define CORE(v) (v)
#define SEC(v)
#define command() ecs_command()
#define claim_result ecs_claim
#define fd_actor ecs_actor
#define current_task ecs_current_task
#define fd_table ecs_table
#define fd_calls ecs_map
#define update ecs_update
#define lookup ecs_lookup
#define result(ticket) ecs_result(ticket)
#define fd_problem ecs_problem
#define fd_stats ecs_stats
#define incarnation ecs_incarnation
#define fd_attach_cookie ecs_attach_cookie
#define fd_file ecs_file
#define bpf_session_is_return ecs_is_return
#define bpf_session_cookie ecs_cookie
#define fd_function_ip ecs_ip
#define fd_read_kernel ecs_read
#define publish_result ecs_publish
static int fd_original_recv_syscall_entered(u64 *ctx,const struct ap_task_command *c) {
    (void)ctx;(void)c;assert(!"receive path outside epoll callback fixture");return -1;
}
#include "epoll-ctl-copy.bpf.h"
#undef pt_regs
#undef task_struct
#undef CORE
#undef SEC
#undef command
#undef claim_result
#undef fd_actor
#undef current_task
#undef fd_table
#undef fd_calls
#undef update
#undef lookup
#undef result
#undef fd_problem
#undef fd_stats
#undef incarnation
#undef fd_attach_cookie
#undef fd_file
#undef bpf_session_is_return
#undef bpf_session_cookie
#undef fd_function_ip
#undef fd_read_kernel
#undef publish_result
static void ecs_prepare(u64 ticket,int op,u64 scratch) {
    memset(&ecs,0,sizeof(ecs));
    ecs.owner=(struct ap_task_command){.provider=3,.command=ticket,.operation=AP_EPOLL_CTL_COPY,
        .expected_object=11,.generation_before=0x10000000004ULL,.generation_after=17,
        .expected_level=op,.expected_option=AP_EPOLL_CTL_SYSCALL};
    ecs.result=(struct ap_command_result){.command=ticket,.operation=AP_EPOLL_CTL_COPY,
        .phase=AP_COMMAND_READY};
    ecs.task=(struct ecs_task){.files=(void *)0x1000,.mm=(void *)0x2000};
    ecs.user=(struct ecs_regs){.orig_ax=AP_EPOLL_CTL_SYSCALL,.cs=0x33,.di=~0ULL,
        .si=(u64)(s64)op,.dx=~0ULL,.r10=ecs.owner.generation_before};
    ecs.stack.caller=0xffffffff81facabcULL;
    for(unsigned i=0;i<12;i++)ecs.stack.event[i]=(u8)(0xa0+i);
    ecs.ctl=(struct ecs_regs){.di=~0ULL,.si=(u64)(s64)op,.dx=~0ULL,
        .sp=(u64)(uintptr_t)&ecs.stack.caller,.cx=(u64)(uintptr_t)ecs.stack.event};
    ecs.ip=0xffffffff81fb1ca0ULL;ecs.cookie=scratch;
    u64 entry[2]={(u64)(uintptr_t)&ecs.user,AP_EPOLL_CTL_SYSCALL};
    assert(fd_epoll_ctl_syscall_entered(entry)==0);
    assert(ecs.present && !ecs.problem && ecs.result.phase==AP_COMMAND_RUNNING);
}
#ifndef AP_FTRACE_PROVIDER
static void ecs_prepare_original(int op) {
    ecs_prepare(47,op,~0ULL);
    ecs.present=false;memset(&ecs.call,0,sizeof(ecs.call));
    ecs.owner.operation=AP_ORIGINAL_EPOLL_CTL;
    ecs.owner.expected_level=3;ecs.owner.expected_option=op;ecs.owner.original_count=4;
    ecs.result=(struct ap_command_result){.command=47,.operation=AP_ORIGINAL_EPOLL_CTL,
        .phase=AP_COMMAND_READY,.original_count=4};
    ecs.user.di=3;ecs.user.dx=4;ecs.ctl.di=3;ecs.ctl.dx=4;ecs.status.next_event=5;
    u64 entry[2]={(u64)(uintptr_t)&ecs.user,AP_EPOLL_CTL_SYSCALL};
    assert(fd_epoll_ctl_syscall_entered(entry)==0);
    assert(ecs.present && !ecs.problem && ecs.call.original.epoll_ctl.before==5);
    assert(fd_epoll_ctl_session(&ecs.ctl)==0);
    assert(ecs.call.original.epoll_ctl.ctl_entered==1 && ecs.cookie==47);
    ecs.ctl.bp=(u64)(uintptr_t)ecs.stack.event;ecs.ctl.bx=(u64)(u32)op;ecs.ctl.r13=4;
}
static void ecs_original_post(unsigned primary,u64 word) {
    ecs.attach_cookie=primary?AP_EPOLL_PRIMARY_POST_COOKIE:AP_EPOLL_TARGET_POST_COOKIE;
    ecs.ctl.ip=ecs.ip+(primary?AP_EPOLL_PRIMARY_POST_OFFSET:AP_EPOLL_TARGET_POST_OFFSET)+1;
    ecs.ctl.ax=word;ecs.status.next_event=primary?6:9;
    assert(fd_original_epoll_post(&ecs.ctl)==0);
}
static void epoll_ctl_original_callback_controls(void) {
    unsigned complete=0,refused=0;
    for(unsigned path=0;path<4;path++) {
        int op=path==3?AP_EPOLL_CTL_DEL:1;ecs_prepare_original(op);
        ecs_original_post(1,path==1?0:0x8001);
        assert(!ecs.problem && !ecs.call.original.problem);
        if(path!=1) {
            assert(!ecs.call.original.selection.ready);
            ecs_original_post(0,path==2?0:0x9000);
        }
        assert(ecs.call.original.selection.ready==1 && !ecs.published);
        assert(ecs.call.original.epoll_ctl.primary_cut==6);
        assert(ecs.call.original.epoll_ctl.target_cut==(path==1?0:9));
        assert(ecs.call.original.selection.file==(path==1?0:31));
        assert(ecs.call.original.epoll_ctl.target_file==((path==1 || path==2)?0:37));
        const s64 returned=(path==1 || path==2)?-9:0;
        memset(ecs.stack.event,0x5a,12);ecs.returning=true;ecs.ctl.ax=(u64)returned;
        assert(fd_epoll_ctl_session(&ecs.ctl)==0);
        assert(ecs.call.original.epoll_ctl.ctl_returned==1 && !ecs.call.original.problem);
        u64 exit[2]={(u64)(uintptr_t)&ecs.user,(u64)returned};
        assert(fd_original_epoll_returned(exit,&ecs.owner)==0 && ecs.published==1);
        assert(ap_original_result_matches(&ecs.owner,&ecs.result,&ecs.call.original));
        for(unsigned i=0;i<12;i++)assert(ecs.call.original.epoll_ctl.event[i]==
            (u8)(op==AP_EPOLL_CTL_DEL?0:0xa0+i));
        complete++;
    }
    for(unsigned bad=0;bad<11;bad++) {
        ecs_prepare_original(1);ecs.attach_cookie=AP_EPOLL_PRIMARY_POST_COOKIE;
        ecs.ctl.ip=ecs.ip+AP_EPOLL_PRIMARY_POST_OFFSET+1;ecs.ctl.ax=0x8001;
        switch(bad) {
        case 0:ecs.attach_cookie=AP_EPOLL_TARGET_POST_COOKIE;break;
        case 1:ecs.ctl.ip++;break;
        case 2:ecs.ctl.bp++;break;
        case 3:ecs.ctl.bx++;break;
        case 4:ecs.ctl.r13++;break;
        case 5:ecs.ctl.r12=1;break;
        case 6:ecs.ctl.ax=0x8002;break;
        case 7:ecs.ctl.ax=1;break;
        case 8:ecs.status.problem=1;break;
        case 9:ecs.ctl.ax=0xa001;break;
        case 10:ecs.call.raw_table++;break;
        }
        assert(fd_original_epoll_post(&ecs.ctl)==0);
        assert(ecs.problem || ecs.call.original.problem);
        assert(!ecs.call.original.selection.ready && !ecs.call.original.epoll_ctl.primary_selected &&
            !ecs.call.original.epoll_ctl.secondary_selected && !ecs.published);
        refused++;
    }
    assert(complete==4 && refused==11);
    printf("original epoll ctl actual callback host controls: complete=%u refused=%u\n",complete,refused);
}
#endif
static void epoll_ctl_session_controls(void) {
    unsigned positives=0,entry_refusals=0,return_refusals=0;
    const u64 scratch[]={0,1,~0ULL,0xffa0000073983e20ULL};
    const int ops[]={1,AP_EPOLL_CTL_DEL,12345};
    for(unsigned s=0;s<4;s++)for(unsigned o=0;o<3;o++) {
        u64 ticket=7+positives;
        ecs_prepare(ticket,ops[o],scratch[s]);
        assert(fd_epoll_ctl_session(&ecs.ctl)==0);
        assert(ecs.cookie==ticket && ecs.call.epoll_copy.ctl_entered==1 &&
            !ecs.call.epoll_copy.ctl_returned && !ecs.call.epoll_copy.problem);
        assert(ecs.reads==(ops[o]==AP_EPOLL_CTL_DEL?1u:2u));
        for(unsigned i=0;i<12;i++)assert(ecs.call.epoll_copy.event[i]==
            (u8)(ops[o]==AP_EPOLL_CTL_DEL?0:0xa0+i));
        memset(ecs.stack.event,0x5a,12);
        ecs.returning=true;ecs.ctl.ax=(u64)(s64)-9;
        assert(fd_epoll_ctl_session(&ecs.ctl)==0);
        assert(ecs.cookie==ticket && ecs.call.epoll_copy.ctl_returned==1 &&
            !ecs.call.epoll_copy.problem && !ecs.published);
        u64 exit[2]={(u64)(uintptr_t)&ecs.user,(u64)(s64)-9};
        assert(fd_epoll_ctl_returned(exit,&ecs.owner)==0 && ecs.published==1);
        assert(ap_epoll_ctl_copy_matches(&ecs.owner,&ecs.result,&ecs.call.epoll_copy));
        for(unsigned i=0;i<12;i++)assert(ecs.call.epoll_copy.event[i]==
            (u8)(ops[o]==AP_EPOLL_CTL_DEL?0:0xa0+i));
        positives++;
    }
    /* Nonzero scratch must not mask a genuinely invalid identity, an earlier
     * entry, wrong caller/operands or failed kernel copy. No return is armed. */
    for(unsigned bad=0;bad<19;bad++) {
        ecs_prepare(29,1,~0ULL);
        switch(bad) {
        case 0:ecs.call.epoll_copy.ctl_entered=1;break;
        case 1:ecs.call.epoll_copy.ctl_returned=1;break;
        case 2:ecs.ctl.sp=0;break;
        case 3:ecs.read_fault=1;break;
        case 4:ecs.stack.caller++;break;
        case 5:ecs.ctl.cx++;break;
        case 6:ecs.ctl.di=4;break;
        case 7:ecs.ctl.si=3;break;
        case 8:ecs.ctl.dx=4;break;
        case 9:ecs.ctl.r8=1;break;
        case 10:ecs.call.raw_table++;break;
        case 11:ecs.call.new_file++;break;
        case 12:ecs.result.task++;break;
        case 13:ecs.result.start_boottime++;break;
        case 14:ecs.call.epoll_copy.call++;break;
        case 15:ecs.call.epoll_copy.owner_mm++;break;
        case 16:ecs.call.command++;break;
        case 17:ecs.read_fault=2;break;
        case 18:ecs.ip++;break;
        }
        assert(fd_epoll_ctl_session(&ecs.ctl)==1);
        assert(ecs.cookie==~0ULL && !ecs.published && !ecs.call.epoll_copy.complete);
        assert(ecs.problem || ecs.call.epoll_copy.problem);entry_refusals++;
    }
    /* Once authenticated, the cookie is exact invocation authority. A missing
     * or other ticket cannot certify a return, including after a valid entry. */
    for(unsigned bad=0;bad<10;bad++) {
        ecs_prepare(31,1,~0ULL);assert(fd_epoll_ctl_session(&ecs.ctl)==0);
        ecs.returning=true;ecs.ctl.ax=(u64)(s64)-9;
        switch(bad) {
        case 0:ecs.cookie=0;break;
        case 1:ecs.cookie=30;break;
        case 2:ecs.call.epoll_copy.ctl_entered=0;break;
        case 3:ecs.call.epoll_copy.ctl_returned=1;break;
        case 4:ecs.call.function_ip=0;break;
        case 5:ecs.call.entry_stack=0;break;
        case 6:ecs.ctl.ax=0;break;
        case 7:ecs.result.task++;break;
        case 8:ecs.call.raw_table++;break;
        case 9:ecs.call.epoll_copy.owner_mm++;break;
        }
        u64 cookie=ecs.cookie,returned=ecs.call.epoll_copy.ctl_returned;
        assert(fd_epoll_ctl_session(&ecs.ctl)==0);
        assert(ecs.cookie==cookie && ecs.call.epoll_copy.ctl_returned==returned &&
            !ecs.published && !ecs.call.epoll_copy.complete);
        assert(ecs.problem || ecs.call.epoll_copy.problem);return_refusals++;
    }
    assert(positives==12 && entry_refusals==19 && return_refusals==10);
    printf("epoll session actual callback host controls: positives=%u entry_refusals=%u return_refusals=%u\n",
        positives,entry_refusals,return_refusals);
}


/* A raw sys_exit is not evidence that sys_enter or native uaccess happened.
 * Run the real return callback at the same controlled helper boundary as the
 * session controls, without first invoking the entry callback. */
static void ecs_prepare_unentered(void) {
    memset(&ecs,0,sizeof(ecs));
    ecs.owner=(struct ap_task_command){.provider=3,.command=7,.operation=AP_EPOLL_CTL_COPY,
        .expected_object=11,.generation_before=0x10000000004ULL,.generation_after=17,
        .expected_level=1,.expected_option=AP_EPOLL_CTL_SYSCALL};
    ecs.result=(struct ap_command_result){.command=7,.operation=AP_EPOLL_CTL_COPY,
        .phase=AP_COMMAND_READY};
    ecs.task=(struct ecs_task){.files=(void *)0x1000,.mm=(void *)0x2000};
    ecs.user=(struct ecs_regs){.orig_ax=AP_EPOLL_CTL_SYSCALL,.cs=0x33,.di=~0ULL,
        .si=1,.dx=~0ULL,.r10=ecs.owner.generation_before};
    ecs.stack.caller=0xffffffff81facabcULL;
    for(unsigned i=0;i<12;i++)ecs.stack.event[i]=(u8)(0xa0+i);
    ecs.ctl=(struct ecs_regs){.di=~0ULL,.si=1,.dx=~0ULL,
        .sp=(u64)(uintptr_t)&ecs.stack.caller,.cx=(u64)(uintptr_t)ecs.stack.event};
    ecs.ip=0xffffffff81fb1ca0ULL;ecs.cookie=~0ULL;
}
static void ecs_assert_unentered_exit_refused(void) {
    struct ap_command_result result=ecs.result;
    struct ap_fd_call call=ecs.call;
    u64 exit[2]={(u64)(uintptr_t)&ecs.user,(u64)(s64)-1};
    assert(fd_epoll_ctl_returned(exit,&ecs.owner)==0);
    assert(ecs.problem==AP_FD_OUTCOME && !ecs.reads && !ecs.published &&
           !memcmp(&ecs.result,&result,sizeof(result)) && !memcmp(&ecs.call,&call,sizeof(call)));
}
static void epoll_ctl_unentered_exit_controls(void) {
    unsigned untouched=0,malformed=0,stale=0,entered=0;
    const int ops[]={1,AP_EPOLL_CTL_DEL,12345};
    const s64 raw[]={-1,-9,-14,0,0x100000003LL};
    for(unsigned o=0;o<3;o++)for(unsigned n=0;n<5;n++) {
        ecs_prepare_unentered();ecs.owner.expected_level=ops[o];ecs.user.si=(u64)(s64)ops[o];
        unsigned char before[sizeof(ecs)];memcpy(before,&ecs,sizeof(ecs));
        u64 exit[2]={(u64)(uintptr_t)&ecs.user,(u64)raw[n]};
        assert(fd_epoll_ctl_returned(exit,&ecs.owner)==0);
        assert(!memcmp(before,&ecs,sizeof(ecs))); /* no claim/copy/result/health change */
        assert(fd_epoll_ctl_returned(exit,&ecs.owner)==0);
        assert(!memcmp(before,&ecs,sizeof(ecs)));untouched++;
    }
    /* The exact ABI reservation is canonical, not merely phase==READY. Every
     * one-byte corruption, including reserved payload, must still refuse. */
    assert(sizeof(struct ap_command_result)==136);
    for(unsigned byte=0;byte<sizeof(struct ap_command_result);byte++) {
        ecs_prepare_unentered();((unsigned char *)&ecs.result)[byte]^=1;
        ecs_assert_unentered_exit_refused();malformed++;
    }
    for(unsigned bad=0;bad<13;bad++) {
        ecs_prepare_unentered();
        switch(bad) {
        case 0:ecs.owner.provider=0;break;
        case 1:ecs.owner.command=0;break;
        case 2:ecs.owner.operation=AP_ORIGINAL_READ;break;
        case 3:ecs.owner.expected_object=0;break;
        case 4:ecs.owner.expected_option=0;break;
        case 5:ecs.owner.original_count=1;break;
        case 6:ecs.user.cs=0x23;break;
        case 7:ecs.user.di=3;break;
        case 8:ecs.user.si=2;break;
        case 9:ecs.user.dx=3;break;
        case 10:ecs.user.r10^=1ULL<<32;break;
        case 11:ecs.result.phase=AP_COMMAND_DONE;break;
        case 12:ecs.result.phase=AP_COMMAND_RUNNING;break;
        }
        ecs_assert_unentered_exit_refused();malformed++;
    }
    /* Any invocation row alongside READY is stale/inconsistent. RUNNING with
     * its claimed actor but a lost row remains an error, never this exception. */
    for(unsigned bad=0;bad<3;bad++) {
        if(bad==0) {ecs_prepare_unentered();ecs.present=true;}
        else {
            ecs_prepare(7,1,~0ULL);
            if(bad==1)ecs.result=(struct ap_command_result){.command=7,
                .operation=AP_EPOLL_CTL_COPY,.phase=AP_COMMAND_READY};
            else ecs.present=false;
        }
        ecs_assert_unentered_exit_refused();stale++;
    }
    /* A suppressed exit has not consumed the reservation. If the actual entry
     * later occurs, the same production callbacks still require and publish
     * its authentic copied bytes, independent of the earlier raw errno. */
    ecs_prepare_unentered();u64 exit[2]={(u64)(uintptr_t)&ecs.user,(u64)(s64)-1};
    assert(fd_epoll_ctl_returned(exit,&ecs.owner)==0 && !ecs.problem && !ecs.present);
    u64 entry[2]={(u64)(uintptr_t)&ecs.user,AP_EPOLL_CTL_SYSCALL};
    assert(fd_epoll_ctl_syscall_entered(entry)==0 && ecs.present && ecs.result.phase==AP_COMMAND_RUNNING);
    assert(fd_epoll_ctl_session(&ecs.ctl)==0);ecs.returning=true;ecs.ctl.ax=(u64)(s64)-9;
    assert(fd_epoll_ctl_session(&ecs.ctl)==0);exit[1]=(u64)(s64)-9;
    assert(fd_epoll_ctl_returned(exit,&ecs.owner)==0 && ecs.published==1 && !ecs.problem);
    assert(ap_epoll_ctl_copy_matches(&ecs.owner,&ecs.result,&ecs.call.epoll_copy));entered++;
    for(unsigned i=0;i<2;i++) {
        ecs_prepare(7,i?12345:1,~0ULL);exit[1]=(u64)(s64)-14;
        assert(fd_epoll_ctl_returned(exit,&ecs.owner)==0 && ecs.published==1 && !ecs.problem);
        assert(ap_epoll_ctl_copy_matches(&ecs.owner,&ecs.result,&ecs.call.epoll_copy));entered++;
    }
    ecs_prepare_unentered();ecs.user.orig_ax=39;
    unsigned char before[sizeof(ecs)];memcpy(before,&ecs,sizeof(ecs));
    assert(fd_epoll_ctl_returned(exit,&ecs.owner)==0 && !memcmp(before,&ecs,sizeof(ecs)));
    assert(untouched==15 && malformed==149 && stale==3 && entered==3);
    printf("epoll unentered actual callback controls: untouched=%u malformed=%u stale=%u entered=%u unrelated=1\n",
        untouched,malformed,stale,entered);
}

static void original_epoll_ctl_controls(void) {
    struct ap_task_command c={.provider=3,.command=7,.operation=AP_ORIGINAL_EPOLL_CTL,
        .expected_object=11,.generation_after=13,.generation_before=0x1000,
        .expected_level=4,.expected_option=1,.original_count=5};
    struct ap_fd_call call={.command=7,.operation=AP_ORIGINAL_EPOLL_CTL,
        .function_ip=0x100000,.entry_stack=0x200000};
    struct ap_original_result *o=&call.original;
    o->selection=(struct ap_original_selection){.command=7,.call=11,.owner_mm=13,.provider=3,
        .task=17,.task_start=19,.table=23,.requested_fd=4,.address_length=1,
        .user_address=0x1000,.original_count=5};
    o->epoll_ctl=(struct ap_original_epoll_ctl){.entered=1,.ctl_entered=1,.before=29,.image_wakeup_policy=1};
    unsigned checks=0;
#define CTL_CHECK(v) do {assert(v);checks++;} while(0)
    CTL_CHECK(ap_original_epoll_ctl_operands(&c,233,0x33,4,1,5,0x1000));
    CTL_CHECK(!ap_original_epoll_ctl_operands(&c,233,0x33,-1,1,-1,0x1000)); // Op13 cannot stand in.
    CTL_CHECK(ap_original_epoll_post_site(&call,&c,18,0x100024,0x200008,1,5,0,0x300001));
    CTL_CHECK(!ap_original_epoll_post_site(&call,&c,19,0x100038,0x200008,1,5,0,0x400001));
    for(unsigned bad=0;bad<8;bad++) {
        u64 cookie=18,ip=0x100024,event=0x200008,op=1,fd=5,nonblock=0,word=0x300001;
        if(bad==0)cookie=17;if(bad==1)ip++;if(bad==2)event++;if(bad==3)op++;
        if(bad==4)fd++;if(bad==5)nonblock=1;if(bad==6)word=0x300003;if(bad==7)word=1;
        CTL_CHECK(!ap_original_epoll_post_site(&call,&c,cookie,ip,event,op,fd,nonblock,word));
    }
    o->selection.file=31;o->selection.fdput_flags=1;o->epoll_ctl.primary_selected=1;o->epoll_ctl.primary_cut=30;
    CTL_CHECK(!ap_original_epoll_post_site(&call,&c,18,0x100024,0x200008,1,5,0,0x300001));
    CTL_CHECK(ap_original_epoll_post_site(&call,&c,19,0x100038,0x200008,1,5,0,0)); // Actual empty target is legal evidence.
    o->epoll_ctl.secondary_selected=1;o->epoll_ctl.target_cut=32;o->epoll_ctl.target_file=37;
    o->epoll_ctl.target_flags=1;o->selection.ready=1;
    o->epoll_ctl.event[0]=1;for(unsigned n=4;n<12;n++)o->epoll_ctl.event[n]=(u8)(n+0x80);
    CTL_CHECK(ap_original_epoll_ctl_selected(&c,o));
    CTL_CHECK(!ap_original_epoll_post_site(&call,&c,19,0x100038,0x200008,1,5,0,0x400001));
    struct ap_command_result r={.command=7,.operation=AP_ORIGINAL_EPOLL_CTL,.phase=AP_COMMAND_DONE,
        .task=17,.start_boottime=19,.identity.provider=3,.original_count=5};
    o->complete=1;o->epoll_ctl.ctl_returned=1;
    CTL_CHECK(ap_original_result_matches(&c,&r,o));
    struct ap_original_result valid=*o;
    for(unsigned bad=0;bad<10;bad++) {
        *o=valid;
        if(bad==0)o->epoll_ctl.secondary_selected=0;if(bad==1)o->epoll_ctl.target_cut=29;
        if(bad==2)o->epoll_ctl.primary_cut=28;if(bad==3)o->epoll_ctl.entered=0;
        if(bad==4)o->epoll_ctl.ctl_returned=0;if(bad==5)o->epoll_ctl.ctl_result=-9;
        if(bad==6)o->epoll_ctl.target_flags=2;if(bad==7)o->address[127]=1;
        if(bad==8)o->selection.original_count++;if(bad==9)o->selection.owner_mm++;
        CTL_CHECK(!ap_original_result_matches(&c,&r,o));
    }
    *o=valid;o->epoll_ctl.target_file=0;o->epoll_ctl.target_flags=0;
    CTL_CHECK(!ap_original_result_matches(&c,&r,o)); // Empty target cannot complete success.
    o->returned=r.returned=o->epoll_ctl.ctl_result=-9;
    CTL_CHECK(ap_original_result_matches(&c,&r,o));
    o->selection.file=0;o->selection.fdput_flags=0;
    CTL_CHECK(!ap_original_result_matches(&c,&r,o)); // Primary empty forbids second lookup.
    o->epoll_ctl.secondary_selected=0;o->epoll_ctl.target_cut=0;
    CTL_CHECK(ap_original_result_matches(&c,&r,o));
    c.expected_option=o->selection.address_length=2;
    CTL_CHECK(!ap_original_result_matches(&c,&r,o)); // DEL never copied event.
    memset(o->epoll_ctl.event,0,12);CTL_CHECK(ap_original_result_matches(&c,&r,o));
    memset(&o->epoll_ctl,0,sizeof(o->epoll_ctl));o->epoll_ctl.entered=1;o->epoll_ctl.image_wakeup_policy=1;
    c.expected_option=o->selection.address_length=1;o->returned=r.returned=-14;
    CTL_CHECK(ap_original_result_matches(&c,&r,o));
    o->complete=0;CTL_CHECK(!ap_original_epoll_ctl_selected(&c,o));o->complete=1;
    o->epoll_ctl.entered=0;CTL_CHECK(!ap_original_result_matches(&c,&r,o));
    printf("original epoll_ctl actual selection/copy/result: %u checks\n",checks);
#undef CTL_CHECK
}

static void auxiliary_file_controls(void) {
    struct ap_task_command c=file_owner();c.operation=AP_AUXILIARY_FILE;
    struct ap_fd_call call=file_entering();call.operation=AP_AUXILIARY_FILE;
    assert(ap_original_operation(c.operation) && ap_original_file_operation(c.operation));
    assert(ap_file_entry_operands(&c,4,72,4,3));
    assert(ap_file_selection_enter(&call,&c,19,23,0x2000,29,12,0x6000));
    assert(ap_file_selection_post(&call,&c,19,23,0x2000,29,13,0x5fe8,4,3,0x8001,0x1077));
    call.selected_file=31;call.original.selection.file=31;
    call.original.selection.fdput_flags=1;call.original.selection.ready=1;
    struct ap_command_result r={.command=7,.operation=AP_AUXILIARY_FILE,.task=19,.start_boottime=23,
        .identity={.provider=3},.phase=AP_COMMAND_RUNNING};
    assert(ap_original_file_native_return(&c,&r,&call,19,23,72,4,3,2048));
    for(unsigned bad=0;bad<5;bad++) {
        struct ap_task_command d=c;struct ap_fd_call f=call;struct ap_command_result q=r;
        if(bad==0)d.operation=AP_ORIGINAL_FILE;if(bad==1)f.operation=AP_ORIGINAL_FILE;
        if(bad==2)q.operation=AP_ORIGINAL_FILE;if(bad==3)d.original_count=1;
        if(bad==4)d.expected_option=4;
        assert(!ap_original_file_native_return(&d,&q,&f,19,23,72,4,3,2048));
    }
    call.original.complete=1;call.original.returned=2048;r.phase=AP_COMMAND_DONE;r.returned=2048;
    assert(ap_original_result_matches(&c,&r,&call.original));
    r.operation=AP_ORIGINAL_FILE;assert(!ap_original_result_matches(&c,&r,&call.original));
    /* A normalized-only auxiliary table never passes a guest lookup/create.
     * The full frozen census is the only caller allowed to request upgrade. */
    assert(ap_fd_table_access(0,AP_TABLE_NORMALIZE)==1);
    assert(ap_fd_table_access(0,AP_TABLE_ANY_LOOKUP)==1);
    assert(!ap_fd_table_access(0,AP_TABLE_ENROLLED_LOOKUP));
    assert(!ap_fd_table_access(0,AP_TABLE_ENROLLED_CREATE));
    assert(ap_fd_table_access(0,AP_TABLE_CENSUS_ENROLL)==2);
    for(unsigned mode=0;mode<=AP_TABLE_CENSUS_ENROLL;mode++)assert(ap_fd_table_access(1,mode)==1);
    for(unsigned mode=0;mode<8;mode++)assert(!ap_fd_table_access(2,mode));
    assert(!ap_fd_table_access(0,5) && !ap_fd_table_access(1,5));
    puts("auxiliary File23: exact fdget/return, role isolation, normalized table admission and sole census upgrade");
}
static void typed_receive_predicates(void) {
    unsigned checks=0;
#define RECV_CHECK(x) do {assert(x);checks++;}while(0)
    for(unsigned peek=0;peek<2;peek++) {
        struct ap_task_command c={.provider=3,.command=7,.operation=peek?AP_ORIGINAL_RECVMSG_CALL:AP_ORIGINAL_RECVFROM_CALL,
            .expected_object=11,.generation_after=13,.generation_before=0x100004000ULL,
            .expected_level=5,.expected_option=peek?0x42:0x40,.original_count=31};
        const u64 nr=peek?47:45,third=peek?0x42:31;
        RECV_CHECK(ap_original_recv_operands(&c,nr,5,c.generation_before,third,0x40,0,0));
        for(unsigned bad=0;bad<(peek?8:11);bad++) {
            struct ap_task_command x=c;u64 n=nr,fd=5,ptr=c.generation_before,arg=third,flags=0x40,addr=0,len=0;
            if(bad==0)n++;if(bad==1)fd++;if(bad==2)ptr^=1ULL<<32;if(bad==3)arg++;
            if(bad==4)x.provider=0;if(bad==5)x.command=0;if(bad==6)x.expected_object=0;
            if(bad==7)x.expected_option=0;if(bad==8)flags=0;if(bad==9)addr=1;if(bad==10)len=1;
            RECV_CHECK(!ap_original_recv_operands(&x,n,fd,ptr,arg,flags,addr,len));
        }
        struct ap_fd_call call={.operation=c.operation,.command=7,.selected_file=29,.selection={.word=0x8000}};
        call.original.selection=(struct ap_original_selection){.provider=3,.command=7,.call=11,.owner_mm=13,
            .task=17,.task_start=19,.table=23,.file=29,.ready=1,.requested_fd=5,
            .user_address=c.generation_before,.address_length=c.expected_option,.original_count=31};
        struct ap_command_result r={.operation=c.operation,.command=7,.phase=AP_COMMAND_RUNNING,.task=17,.start_boottime=19,
            .identity={.provider=3},.original_count=31};
        RECV_CHECK(ap_original_recv_context(&c,&r,&call,17,19));
        for(unsigned bad=0;bad<10;bad++) {
            struct ap_fd_call x=call;
            if(bad==0)x.selection.entered=1;if(bad==1)x.selection.returned=1;if(bad==2)x.selection.word|=1;
            if(bad==3)x.original.selection.fdput_flags=1;if(bad==4)x.original.selection.ready=0;
            if(bad==5)x.original.selection.address_length^=2;if(bad==6)x.selected_file++;
            if(bad==7)x.file_entry_ip=1;if(bad==8)x.entry_stack=1;if(bad==9)x.original.selection.file=0;
            RECV_CHECK(!ap_original_recv_context(&c,&r,&x,17,19));
        }
    }
    /* The standalone control is built once for each exact selected ABI. */
    struct ap_fd_call copy={0};
#if AP_NATIVE_COPY_VERSION == 5
    copy.original.stream_copy.summary.version=4;RECV_CHECK(!ap_original_recv_copy_version(&copy));
    copy.original.stream_copy.summary.version=5;RECV_CHECK(ap_original_recv_copy_version(&copy));
#else
    copy.original.stream_copy.summary.version=4;RECV_CHECK(ap_original_recv_copy_version(&copy));
    copy.original.stream_copy.summary.version=5;RECV_CHECK(!ap_original_recv_copy_version(&copy));
#endif
    assert(checks==45);printf("typed helper receive operands/issuer/copy version: %u checks\n",checks);
#undef RECV_CHECK
}
int main(void) {
#ifdef AP_FTRACE_PROVIDER
    (void)ecs_attach_cookie;
    (void)fd_original_epoll_apply_post;
    (void)fd_original_epoll_returned;
#endif
    typed_receive_predicates();
    original_epoll_ctl_controls();
    original_epoll_allocator_controls();
    auxiliary_file_controls();
    original_openat_controls();
    epoll_ctl_unentered_exit_controls();
    epoll_ctl_session_controls();
#ifndef AP_FTRACE_PROVIDER
    epoll_ctl_original_callback_controls();
#endif
    epoll_ctl_copy_controls();
    original_socket_controls();
    original_file_caller_controls();
    original_scalar_read_controls();
    original_file_kernel_read_controls();
    inline_file_selection_controls();
    original_file_controls();
    fdupfd_interval_controls();
    original_fdget_frame_controls();
    original_fdget_controls();
    struct ap_task_command c={.provider=3,.command=7,.operation=AP_ACCEPT_EFFECT,
        .expected_object=11,.generation_before=19,.generation_after=0,
        .expected_level=5,.expected_option=0};
    struct ap_command_result r={.command=7,.operation=AP_ACCEPT_EFFECT,.task=23,
        .start_boottime=29,.identity={3,31,37},.creation=41,.cookie=43,
        .returned=8,.phase=AP_COMMAND_DONE};
    struct ap_fd_accept a={.command=7,.accept_lease=19,.owner_mm=0,.task=23,
        .task_start=29,.table=47,.file=53,.install_begin=59,.install_end=61,
        .listener={3,11,37},.child={3,31,37},.creation=41,.cookie=43,
        .phases=127,.requested_fd=5,.flags=0,.returned_fd=8};
    unsigned checks=0;
#define CHECK(v) do { assert(v);checks++; } while(0)
    CHECK(ap_fd_accept_matches(&c,&r,&a));
    /* Actual successful installation is historical. A later REMOVE is a
     * separate journal fact, never permission to rewrite this return to errno. */
    struct ap_fd_event removed={.sequence=60,.kind=AP_FD_REMOVE,.table=47,.fd=8,.file=53,.complete=1};
    CHECK(removed.sequence<a.install_end && removed.file==a.file);
    CHECK(ap_fd_accept_matches(&c,&r,&a));
#define BAD(field,value) do { struct ap_fd_accept b=a;b.field=(value);CHECK(!ap_fd_accept_matches(&c,&r,&b)); } while(0)
    BAD(command,8);BAD(accept_lease,20);BAD(owner_mm,1);BAD(task,24);BAD(task_start,30);
    BAD(table,0);BAD(file,0);BAD(install_begin,0);BAD(install_end,0);
    BAD(listener.object,12);BAD(child.object,32);BAD(child.namespace,38);
    BAD(creation,42);BAD(cookie,44);BAD(phases,63);BAD(phases,255);BAD(problem,AP_FD_IDENTITY);
    BAD(requested_fd,6);BAD(flags,2);BAD(returned_fd,9);BAD(do_accept_errno,14);
    struct ap_fd_accept failure=a;struct ap_command_result error=r;
    failure.phases=AP_FD_ENTERED|AP_FD_LISTENER|AP_FD_DEQUEUED|AP_FD_SYSCALL_RETURNED;
    failure.file=failure.install_begin=failure.install_end=0;
    failure.do_accept_errno=14;error.returned=-14;
    CHECK(ap_fd_accept_matches(&c,&error,&failure));
    failure.phases|=AP_FD_INSTALL_ENTERED;
    CHECK(!ap_fd_accept_matches(&c,&error,&failure));
    failure.phases=AP_FD_ENTERED|AP_FD_SYSCALL_RETURNED;
    failure.child=(struct ap_identity){0};failure.creation=failure.cookie=0;
    failure.do_accept_errno=0;error.identity=(struct ap_identity){.provider=3};
    error.creation=error.cookie=0;error.returned=-9;
    CHECK(ap_fd_accept_matches(&c,&error,&failure));
    error.returned=-4096;
    CHECK(!ap_fd_accept_matches(&c,&error,&failure));
    printf("fd-effect predicates: %u checks\n",checks);
    unsigned selection_checks=0;
#define SELECT_CHECK(v) do { assert(v);selection_checks++; } while(0)
    const u64 valid_words[]={0,0x1000,0x1001};
    for(unsigned i=0;i<3;i++) {
        struct ap_fd_selection s={0};
        SELECT_CHECK(!ap_fd_selection_complete(&s));
        SELECT_CHECK(!ap_fd_selection_return(&s,valid_words[i]));
        SELECT_CHECK(ap_fd_selection_enter(&s));
        SELECT_CHECK(!ap_fd_selection_complete(&s));
        SELECT_CHECK(ap_fd_selection_return(&s,valid_words[i]));
        SELECT_CHECK(ap_fd_selection_complete(&s) && s.word==valid_words[i]);
        struct ap_fd_selection exact=s;
        SELECT_CHECK(!ap_fd_selection_enter(&s));
        SELECT_CHECK(!ap_fd_selection_return(&s,valid_words[i]));
        SELECT_CHECK(!memcmp(&s,&exact,sizeof(s)));
    }
    const u64 invalid_words[]={1,2,3,0x1002,0x1003};
    for(unsigned i=0;i<5;i++) {
        struct ap_fd_selection s={0};
        SELECT_CHECK(ap_fd_selection_enter(&s));
        struct ap_fd_selection exact=s;
        SELECT_CHECK(!ap_fd_selection_return(&s,invalid_words[i]));
        SELECT_CHECK(!memcmp(&s,&exact,sizeof(s)) && !ap_fd_selection_complete(&s));
    }
    /* A missing callback and an interrupted attempt cannot be reused as the
     * next native invocation. Its new provider command has fresh state. */
    struct ap_fd_selection missing={.entered=1},fresh={0};
    SELECT_CHECK(!ap_fd_selection_complete(&missing));
    SELECT_CHECK(!ap_fd_selection_enter(&missing));
    SELECT_CHECK(ap_fd_selection_enter(&fresh));
    SELECT_CHECK(ap_fd_selection_return(&fresh,0x2001));
    SELECT_CHECK(ap_fd_selection_complete(&fresh));
    assert(selection_checks==47);
    printf("original fdget selection: %u checks\n",selection_checks);
    unsigned birth_checks=0;
#define BIRTH_CHECK(v) do { assert(v);birth_checks++; } while(0)
    struct ap_task_command bc={.provider=3,.command=7,.operation=AP_NATIVE_BIRTH,
        .expected_object=17,.generation_before=47,.generation_after=23,.expected_level=435};
    struct ap_native_birth br={.command=7,.call=17,.owner_mm=23,.provider=3,
        .creator_task=(11ULL<<32)|11,.creator_start=29,.creator_table=47,
        .child_task=(12ULL<<32)|12,.child_start=31,.child_table=53,
        .parent_task=(11ULL<<32)|11,.parent_start=29,.copy_begin=59,.copy_end=61,
        .ready=1,.exit_signal=17,.requested_exit_signal=17,.pidfd_fd=-1};
    BIRTH_CHECK(ap_native_birth_matches(&bc,&br));
#define BAD_BIRTH(field,value) do { struct ap_native_birth bad=br;bad.field=(value);BIRTH_CHECK(!ap_native_birth_matches(&bc,&bad)); } while(0)
    BAD_BIRTH(command,8);BAD_BIRTH(call,18);BAD_BIRTH(owner_mm,24);BAD_BIRTH(provider,4);
    BAD_BIRTH(creator_task,0);BAD_BIRTH(creator_start,0);BAD_BIRTH(creator_table,48);
    BAD_BIRTH(child_task,br.creator_task);BAD_BIRTH(child_start,0);BAD_BIRTH(child_table,47);
    BAD_BIRTH(parent_task,0);BAD_BIRTH(parent_start,0);BAD_BIRTH(copy_begin,0);BAD_BIRTH(copy_end,59);
    BAD_BIRTH(ready,0);BAD_BIRTH(ready,2);BAD_BIRTH(problem,AP_FD_IDENTITY);
    BAD_BIRTH(shared_files,1);BAD_BIRTH(shared_mm,2);BAD_BIRTH(same_thread_group,1);
    BAD_BIRTH(exit_signal,-2);BAD_BIRTH(requested_exit_signal,65);BAD_BIRTH(pidfd_fd,0);
    struct ap_native_birth shared=br;shared.kernel_flags=AP_CLONE_FILES|AP_CLONE_VM;
    shared.shared_files=shared.shared_mm=1;shared.child_table=47;shared.copy_begin=shared.copy_end=0;
    BIRTH_CHECK(ap_native_birth_matches(&bc,&shared));
    shared.kernel_flags|=AP_CLONE_THREAD;shared.same_thread_group=1;
    shared.child_task=(11ULL<<32)|12;shared.exit_signal=-1;
    BIRTH_CHECK(ap_native_birth_matches(&bc,&shared));
    shared.child_task=(13ULL<<32)|12;BIRTH_CHECK(!ap_native_birth_matches(&bc,&shared));
    struct ap_native_birth pidfd=br;pidfd.kernel_flags=AP_CLONE_PIDFD;
    pidfd.pidfd_install_begin=67;pidfd.pidfd_install_end=71;pidfd.pidfd_file=73;pidfd.pidfd_fd=8;
    BIRTH_CHECK(ap_native_birth_matches(&bc,&pidfd));
    pidfd.pidfd_install_end=0;BIRTH_CHECK(!ap_native_birth_matches(&bc,&pidfd));
    // A chosen-parent candidate is never a successful birth by itself.
    br.ready=0;BIRTH_CHECK(!ap_native_birth_matches(&bc,&br));
    assert(birth_checks==30);
    printf("native birth predicates: %u checks\n",birth_checks);
    unsigned native_return_checks=0;
#define RETURN_CHECK(v) do {assert(v);native_return_checks++;} while(0)
    struct ap_command_result nr={.command=7,.operation=AP_NATIVE_BIRTH,.phase=AP_COMMAND_READY};
    RETURN_CHECK(ap_native_birth_return(&bc,&nr,0,435,-14)==AP_NATIVE_RETURN_EARLY_ERROR);
    RETURN_CHECK(ap_native_birth_return(&bc,&nr,0,435,-22)==AP_NATIVE_RETURN_EARLY_ERROR);
    RETURN_CHECK(ap_native_birth_return(&bc,&nr,0,435,-7)==AP_NATIVE_RETURN_EARLY_ERROR);
    RETURN_CHECK(ap_native_birth_return(&bc,&nr,0,435,0)==AP_NATIVE_RETURN_INVALID);
    RETURN_CHECK(ap_native_birth_return(&bc,&nr,0,435,12)==AP_NATIVE_RETURN_INVALID);
    RETURN_CHECK(ap_native_birth_return(&bc,&nr,0,435,-4096)==AP_NATIVE_RETURN_INVALID);
    RETURN_CHECK(ap_native_birth_return(&bc,&nr,0,435,0x80000000LL)==AP_NATIVE_RETURN_INVALID);
    RETURN_CHECK(ap_native_birth_return(&bc,&nr,0,56,-14)==AP_NATIVE_RETURN_INVALID);
    struct ap_task_command wrong_shape=bc;wrong_shape.expected_level=60;
    RETURN_CHECK(ap_native_birth_return(&wrong_shape,&nr,0,60,-14)==AP_NATIVE_RETURN_INVALID);
    nr.command=8;RETURN_CHECK(ap_native_birth_return(&bc,&nr,0,435,-14)==AP_NATIVE_RETURN_INVALID);nr.command=7;
    nr.phase=AP_COMMAND_DONE;RETURN_CHECK(ap_native_birth_return(&bc,&nr,0,435,-14)==AP_NATIVE_RETURN_INVALID);
    struct ap_fd_call nc={.command=7,.operation=AP_NATIVE_BIRTH,.copied_address=1,.birth=br};
    nr.phase=AP_COMMAND_READY;RETURN_CHECK(ap_native_birth_return(&bc,&nr,&nc,435,-14)==AP_NATIVE_RETURN_INVALID);
    nr.phase=AP_COMMAND_RUNNING;nr.task=br.creator_task;nr.start_boottime=br.creator_start;
    nr.identity.provider=3;nr.returned=12;nc.birth.ready=1;
    RETURN_CHECK(ap_native_birth_return(&bc,&nr,&nc,435,12)==AP_NATIVE_RETURN_KERNEL);
    RETURN_CHECK(ap_native_birth_return(&bc,&nr,0,435,12)==AP_NATIVE_RETURN_INVALID);
    RETURN_CHECK(ap_native_birth_return(&bc,&nr,&nc,435,13)==AP_NATIVE_RETURN_INVALID);
    nc.birth.ready=0;RETURN_CHECK(ap_native_birth_return(&bc,&nr,&nc,435,12)==AP_NATIVE_RETURN_INVALID);
    nr.returned=-11;RETURN_CHECK(ap_native_birth_return(&bc,&nr,&nc,435,-11)==AP_NATIVE_RETURN_KERNEL);
    nc.birth.ready=1;RETURN_CHECK(ap_native_birth_return(&bc,&nr,&nc,435,-11)==AP_NATIVE_RETURN_INVALID);nc.birth.ready=0;
    nc.copied_address=0;RETURN_CHECK(ap_native_birth_return(&bc,&nr,&nc,435,-11)==AP_NATIVE_RETURN_INVALID);nc.copied_address=1;
    nr.task++;RETURN_CHECK(ap_native_birth_return(&bc,&nr,&nc,435,-11)==AP_NATIVE_RETURN_INVALID);nr.task--;
    nc.birth.owner_mm++;RETURN_CHECK(ap_native_birth_return(&bc,&nr,&nc,435,-11)==AP_NATIVE_RETURN_INVALID);nc.birth.owner_mm--;
    nc.birth.problem=AP_FD_OUTCOME;RETURN_CHECK(ap_native_birth_return(&bc,&nr,&nc,435,-11)==AP_NATIVE_RETURN_INVALID);
    assert(native_return_checks==22);
    printf("native syscall-return predicates: %u checks\n",native_return_checks);
    unsigned actor_checks=0;
#define ACTOR_CHECK(v) do {assert(v);actor_checks++;} while(0)
    struct ap_command_result issuer={.command=7,.operation=AP_NATIVE_BIRTH,.phase=AP_COMMAND_READY};
    ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,0,br.creator_task,29)==AP_NATIVE_ACTOR_CREATOR);
    issuer.task=br.creator_task;issuer.start_boottime=29;issuer.phase=AP_COMMAND_RUNNING;
    struct ap_fd_call origin={.command=7,.operation=AP_NATIVE_BIRTH};origin.birth=br;origin.birth.ready=1;
    ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,&origin,br.creator_task,29)==AP_NATIVE_ACTOR_CREATOR);
    ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,&origin,br.child_task,31)==AP_NATIVE_ACTOR_CHILD);
    ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,&origin,br.child_task,32)==AP_NATIVE_ACTOR_INVALID);
    ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,&origin,br.creator_task,30)==AP_NATIVE_ACTOR_INVALID);
    ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,&origin,99,31)==AP_NATIVE_ACTOR_INVALID);
    ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,0,br.child_task,31)==AP_NATIVE_ACTOR_INVALID);
    origin.birth.creator_start++;ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,&origin,br.child_task,31)==AP_NATIVE_ACTOR_INVALID);origin.birth.creator_start--;
    origin.birth.command++;ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,&origin,br.child_task,31)==AP_NATIVE_ACTOR_INVALID);origin.birth.command--;
    origin.birth.provider++;ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,&origin,br.child_task,31)==AP_NATIVE_ACTOR_INVALID);origin.birth.provider--;
    origin.birth.ready=0;ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,&origin,br.child_task,31)==AP_NATIVE_ACTOR_INVALID);origin.birth.ready=1;
    origin.birth.problem=1;ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,&origin,br.child_task,31)==AP_NATIVE_ACTOR_INVALID);origin.birth.problem=0;
    issuer.command++;ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,&origin,br.child_task,31)==AP_NATIVE_ACTOR_INVALID);issuer.command--;
    issuer.phase=AP_COMMAND_DONE;ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,&origin,br.child_task,31)==AP_NATIVE_ACTOR_CHILD);
    issuer.phase=AP_COMMAND_RUNNING;
    ACTOR_CHECK(ap_native_birth_return(&bc,&issuer,&origin,435,0)==AP_NATIVE_RETURN_INVALID);
    issuer.phase=AP_COMMAND_READY;ACTOR_CHECK(ap_native_birth_actor(&bc,&issuer,0,br.creator_task,29)==AP_NATIVE_ACTOR_INVALID);
    assert(actor_checks==16);printf("native birth issuer generation: %u checks\n",actor_checks);
    native_clear_tid_controls(&bc,&br);
    original_copy_controls();
    unsigned held_checks=0;
#define HELD_CHECK(v) do {assert(v);held_checks++;} while(0)
    struct ap_task_command held={.provider=7,.command=41,.operation=AP_OBSERVE_SOCKET_FILE};
    HELD_CHECK(ap_socket_file_observation_command(&held));
    HELD_CHECK(!ap_socket_file_observation_command(0));
    for(unsigned field=0;field<9;field++) {
        struct ap_task_command changed=held;
        switch(field) {
            case 0:changed.provider=0;break;
            case 1:changed.command=0;break;
            case 2:changed.operation=AP_MATCH;break;
            case 3:changed.expected_object=19;break;
            case 4:changed.generation_before=1;break;
            case 5:changed.generation_after=1;break;
            case 6:changed.expected_level=1;break;
            case 7:changed.expected_option=1;break;
            case 8:changed.original_count=1;break;
        }
        HELD_CHECK(!ap_socket_file_observation_command(&changed));
    }
    assert(held_checks==11);printf("held socket observation command: %u checks\n",held_checks);
    return 0;
}
#endif
