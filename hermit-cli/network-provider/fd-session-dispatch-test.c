/* SPDX-License-Identifier: MIT */
/* Host control of the ACTUAL fd_so/fd_si bodies. Kernel/session/map helpers
 * below are explicit boundary doubles, not a verifier or kernel execution. */
#include <assert.h>
#include <stdio.h>
#include <string.h>
#define AP_FTRACE_PROVIDER 1
#define AP_GROUPED_PROVIDER 1
#include "fd-effects.h"
#include "retirement-target.h"
#include "grouped-probes.h"

/* Only pointer forwarding is tested here; no invented register evidence. */
struct pt_regs { unsigned opaque; };
static struct pt_regs context;
enum event { COOKIE=1,BOOTSTRAP,RETURNING,COMMAND,CLAIM,EPOLL,DUPFD,
    FILE_GET,READ_GET,CONNECT_SESSION,ACCEPT,CONNECT,INCARNATION,PROBLEM };
static struct {
    u64 cookie,problem;
    unsigned returning,command_missing,claim_missing,bootstrap_consumed;
    int leaf_return,published;
    unsigned events[32],count,claims;
} trace;
static struct ap_task_command owner;
static struct ap_command_result result_cell;
static void note(unsigned event) {
    assert(trace.count<sizeof(trace.events)/sizeof(trace.events[0]));
    trace.events[trace.count++]=event;
}
static u64 fd_attach_cookie_raw(void *ctx) {
    assert(ctx==&context);note(COOKIE);return trace.cookie;
}
static int fd_grouped_bootstrap(struct pt_regs *ctx,u64 kind) {
    assert(ctx==&context && kind==trace.cookie);note(BOOTSTRAP);
    return (int)trace.bootstrap_consumed;
}
static int bpf_session_is_return(struct pt_regs *ctx) {
    assert(ctx==&context);note(RETURNING);return (int)trace.returning;
}
static struct ap_task_command *command(void) {
    note(COMMAND);return trace.command_missing?NULL:&owner;
}
static struct ap_command_result *claim_result(struct ap_task_command *c) {
    assert(c==&owner);note(CLAIM);trace.claims++;
    /* Deliberate boundary double: the production CAS is NOT tested here. */
    if(trace.claim_missing || trace.claims>1)return NULL;
    return &result_cell;
}
static u64 incarnation(void) {note(INCARNATION);return 73;}
static void fd_problem(u64 problem) {note(PROBLEM);trace.problem|=problem;}
static int leaf(struct pt_regs *ctx,unsigned event) {
    assert(ctx==&context);note(event);return trace.leaf_return;
}
static int fd_epoll_ctl_session(struct pt_regs *ctx) {return leaf(ctx,EPOLL);}
static int fd_fdupfd_session(struct pt_regs *ctx,u64 kind) {
    assert(kind==trace.cookie);return leaf(ctx,DUPFD);
}
static int fd_file_fdget_session(struct pt_regs *ctx) {return leaf(ctx,FILE_GET);}
static int fd_read_fdget_session(struct pt_regs *ctx) {return leaf(ctx,READ_GET);}
static int fd_connect_session(struct pt_regs *ctx) {return leaf(ctx,CONNECT_SESSION);}
static int fd_accept_enter(struct pt_regs *ctx,u64 kind,struct ap_task_command *c) {
    assert(kind==AP_ACCEPT_ENTRY_COOKIE && kind==trace.cookie && c==&owner);
    (void)leaf(ctx,ACCEPT);return trace.published;
}
static int fd_connect_enter(struct pt_regs *ctx,u64 kind,struct ap_task_command *c) {
    assert(kind==AP_CONNECT_ENTRY_COOKIE && kind==trace.cookie && c==&owner);
    (void)leaf(ctx,CONNECT);return trace.published;
}
#define SEC(name)
#ifndef FD_DISPATCH_SOURCE
#define FD_DISPATCH_SOURCE "fd-session-dispatch.inc"
#endif
#include FD_DISPATCH_SOURCE

struct route {u64 cookie;unsigned outer,event;};
/* Independent literal membership matches driver.c's maintained 4+5 groups. */
static const struct route routes[]={
    {6,1,ACCEPT},{7,1,CONNECT},{10,1,DUPFD},{17,1,EPOLL},
    {2,0,CONNECT_SESSION},{4,0,CONNECT_SESSION},{11,0,DUPFD},
    {21,0,FILE_GET},{22,0,READ_GET}
};
static void reset(u64 cookie,unsigned returning) {
    memset(&trace,0,sizeof(trace));memset(&owner,0,sizeof(owner));
    memset(&result_cell,0,sizeof(result_cell));
    trace.cookie=cookie;trace.returning=returning;trace.published=1;
    owner.operation=cookie==6?AP_ACCEPT_EFFECT:AP_ORIGINAL_CONNECT;
    result_cell.identity.provider=19;
}
static int invoke(unsigned outer) {return outer?fd_so(&context):fd_si(&context);}
static void events(const unsigned *expected,unsigned count) {
    assert(trace.count==count);
    assert(!memcmp(trace.events,expected,count*sizeof(*expected)));
}
#define EVENTS(...) do { const unsigned expected[]={__VA_ARGS__}; \
    events(expected,sizeof(expected)/sizeof(expected[0])); } while(0)
static void test_routes(void) {
    for(unsigned i=0;i<9;i++)for(unsigned returning=0;returning<2;returning++) {
        const struct route *r=&routes[i];
        for(int value=0;value<=1;value++) {
            reset(r->cookie,returning);trace.leaf_return=value;
            int rc=invoke(r->outer);
            if(r->cookie==6 || r->cookie==7) {
                assert(rc==1);
                if(returning) {
                    EVENTS(COOKIE,BOOTSTRAP,RETURNING,PROBLEM);
                    assert(trace.problem==AP_FD_IDENTITY);
                    assert(result_cell.identity.provider==19);
                } else {
                    EVENTS(COOKIE,BOOTSTRAP,RETURNING,COMMAND,CLAIM,r->event,INCARNATION);
                    assert(result_cell.identity.provider==73 && !trace.problem);
                }
            } else {
                assert(rc==value);
                if(r->outer) {EVENTS(COOKIE,BOOTSTRAP,r->event);}
                else {EVENTS(COOKIE,r->event);}
                assert(result_cell.identity.provider==19 && !trace.problem);
            }
        }
    }
}
static void no_effect(unsigned outer,u64 cookie,unsigned returning) {
    reset(cookie,returning);
    const struct ap_task_command before_owner=owner;
    const struct ap_command_result before_result=result_cell;
    assert(invoke(outer)==1);
    EVENTS(COOKIE);
    assert(!trace.problem && !trace.claims);
    assert(!memcmp(&owner,&before_owner,sizeof(owner)));
    assert(!memcmp(&result_cell,&before_result,sizeof(result_cell)));
}
static void test_wrong_group(void) {
    for(unsigned i=0;i<9;i++)for(unsigned returning=0;returning<2;returning++)
        no_effect(!routes[i].outer,routes[i].cookie,returning);
}
static void test_unknown(void) {
    for(unsigned outer=0;outer<2;outer++)for(unsigned returning=0;returning<2;returning++) {
        for(u64 cookie=0;cookie<=24;cookie++) {
            unsigned member=0;
            for(unsigned i=0;i<9;i++)member|=routes[i].outer==outer && routes[i].cookie==cookie;
            if(!member)no_effect(outer,cookie,returning);
        }
        no_effect(outer,AP_GROUPED_COOKIE,returning);
        no_effect(outer,UINT64_MAX,returning);
        for(unsigned i=0;i<9;i++) {
            no_effect(outer,routes[i].cookie|(UINT64_C(1)<<32),returning);
            no_effect(outer,routes[i].cookie|(UINT64_C(1)<<63),returning);
        }
    }
}
static void test_missing(void) {
    for(u64 cookie=6;cookie<=7;cookie++) {
        reset(cookie,0);trace.command_missing=1;
        assert(fd_so(&context)==1);EVENTS(COOKIE,BOOTSTRAP,RETURNING,COMMAND);
        assert(result_cell.identity.provider==19 && !trace.problem);
        reset(cookie,0);owner.operation=AP_ORIGINAL_READ;
        assert(fd_so(&context)==1);EVENTS(COOKIE,BOOTSTRAP,RETURNING,COMMAND);
        assert(result_cell.identity.provider==19 && !trace.problem);
        reset(cookie,0);trace.claim_missing=1;
        assert(fd_so(&context)==1);EVENTS(COOKIE,BOOTSTRAP,RETURNING,COMMAND,CLAIM);
        assert(result_cell.identity.provider==19 && !trace.problem);
        reset(cookie,0);trace.published=0;
        assert(fd_so(&context)==1);
        EVENTS(COOKIE,BOOTSTRAP,RETURNING,COMMAND,CLAIM,cookie==6?ACCEPT:CONNECT);
        assert(result_cell.identity.provider==19 && !trace.problem);
    }
}
static void test_duplicate(void) {
    for(u64 cookie=6;cookie<=7;cookie++) {
        reset(cookie,0);assert(fd_so(&context)==1);
        assert(trace.claims==1 && result_cell.identity.provider==73);
        const struct ap_command_result before=result_cell;
        trace.count=0;
        assert(fd_so(&context)==1);EVENTS(COOKIE,BOOTSTRAP,RETURNING,COMMAND,CLAIM);
        assert(trace.claims==2 && !memcmp(&before,&result_cell,sizeof(before)));
    }
}
static void test_bootstrap(void) {
    reset(7,0);trace.bootstrap_consumed=1;
    assert(fd_so(&context)==1);EVENTS(COOKIE,BOOTSTRAP);
    assert(result_cell.identity.provider==19 && !trace.claims);
    reset(7,1);trace.bootstrap_consumed=1;trace.problem=AP_FD_DUPLICATE;
    assert(fd_so(&context)==1);EVENTS(COOKIE,BOOTSTRAP);
    assert(trace.problem==AP_FD_DUPLICATE && result_cell.identity.provider==19);
    /* An armed/consuming modeled bootstrap cannot run on any inner route. */
    for(unsigned i=4;i<9;i++) {
        reset(routes[i].cookie,0);trace.bootstrap_consumed=1;
        assert(fd_si(&context)==0);EVENTS(COOKIE,routes[i].event);
    }
}
static void test_metadata(void) {
    for(unsigned group=0;group<2;group++) {
        const unsigned count=group?5:4,base=group?4:0;
        uint64_t addresses[5]={0},cookies[5]={0},want_addresses[5]={0},want_cookies[5]={0};
        struct bpf_prog_info program={.id=31,.type=BPF_PROG_TYPE_KPROBE};
        struct bpf_link_info link={.id=37,.prog_id=31,.type=BPF_LINK_TYPE_KPROBE_MULTI};
        link.kprobe_multi.count=count;
        for(unsigned i=0;i<count;i++) {
            want_addresses[i]=0x1000+0x100*i;want_cookies[i]=routes[base+i].cookie;
            addresses[count-1-i]=want_addresses[i];cookies[count-1-i]=want_cookies[i];
        }
#define MATCH() ap_ftrace_kprobe_multi_link_matches(&program,sizeof(program),&link,sizeof(link), \
    addresses,cookies,want_addresses,want_cookies,count,0)
        assert(MATCH());
        const uint64_t saved=cookies[0];
        cookies[0]=routes[group?0:4].cookie;assert(!MATCH());
        cookies[0]=saved|(UINT64_C(1)<<32);assert(!MATCH());
        cookies[0]=saved|(UINT64_C(1)<<63);assert(!MATCH());
        cookies[0]=AP_GROUPED_COOKIE;assert(!MATCH());
        cookies[0]=saved;
        link.kprobe_multi.count=count-1;assert(!MATCH());link.kprobe_multi.count=count;
        const uint64_t saved_address=addresses[0];
        addresses[0]=addresses[1];cookies[0]=cookies[1];assert(!MATCH());
        addresses[0]=saved_address;cookies[0]=saved;
        program.recursion_misses=1;assert(!MATCH());program.recursion_misses=0;
        link.kprobe_multi.missed=1;assert(!MATCH());link.kprobe_multi.missed=0;
        assert(MATCH());
#undef MATCH
    }
}
static void test_ordinary(void) {
    reset(17,0);assert(fd_so(&context)==0);EVENTS(COOKIE,BOOTSTRAP,EPOLL);
    assert(!trace.problem && result_cell.identity.provider==19);
}
static int fd_session_dispatch_run(int argc,char **argv) {
    (void)ap_require_retirement_target;
    assert(argc==2);
    const struct {const char *name;void (*run)(void);} tests[]={
        {"routes",test_routes},{"wrong-group",test_wrong_group},{"unknown",test_unknown},
        {"missing",test_missing},{"duplicate",test_duplicate},{"bootstrap",test_bootstrap},
        {"metadata",test_metadata},{"ordinary",test_ordinary}
    };
    unsigned ran=0;
    for(unsigned i=0;i<sizeof(tests)/sizeof(tests[0]);i++)
        if(!strcmp(argv[1],"all") || !strcmp(argv[1],tests[i].name)) {tests[i].run();ran++;}
    assert(ran==(!strcmp(argv[1],"all")?8U:1U));
    printf("fd-session dispatch controls: %u; modeled helper boundary\n",ran);
    return 0;
}
void fd_session_dispatch_controls(void) {
    char *argv[]={"fd-session-dispatch-test","all"};
    int rc=fd_session_dispatch_run(2,argv);
    assert(rc==0);
}
#ifndef AP_FD_SESSION_DISPATCH_EMBEDDED
int main(int argc,char **argv) {
    return fd_session_dispatch_run(argc,argv);
}
#endif
