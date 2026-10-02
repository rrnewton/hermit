/* SPDX-License-Identifier: BSD-3-Clause */
/* Included only by the explicit grouped provider translation unit. */
#include "grouped-target.h"
#ifndef AP_FTRACE_PROVIDER
#include <linux/perf_event.h>
#include <time.h>
struct ap_perf_event_opts {
    size_t sz;u64 bpf_cookie;bool force_ioctl,dont_enable;size_t :0;
};
_Static_assert(sizeof(struct ap_perf_event_opts)==24,"installed libbpf1.6.3 perf opts");
_Static_assert(offsetof(struct ap_perf_event_opts,bpf_cookie)==8,"perf cookie");
_Static_assert(offsetof(struct ap_perf_event_opts,force_ioctl)==16,"no ioctl fallback");
_Static_assert(offsetof(struct ap_perf_event_opts,dont_enable)==17,"perf enable selector");
extern struct bpf_link *bpf_program__attach_perf_event_opts(const struct bpf_program *,int,const struct ap_perf_event_opts *);
static int grouped_deadline(u64 *deadline) {
    struct timespec t;if(clock_gettime(CLOCK_MONOTONIC,&t))return -1;
    if(t.tv_sec<0 || (u64)t.tv_sec>(~0ULL-2000000000ULL)/1000000000ULL ||
       t.tv_nsec<0 || t.tv_nsec>=1000000000)return unavailable();
    *deadline=(u64)t.tv_sec*1000000000ULL+(u64)t.tv_nsec+2000000000ULL;return 0;
}
#endif
static u64 grouped_runtime_address(u64 anchor,u64 image) {
    if(!ap_grouped_site_ip(1,anchor))return 0;
    if(image>=AP_GROUPED_CONNECT_IMAGE) {
        u64 delta=image-AP_GROUPED_CONNECT_IMAGE;
        return anchor<=~0ULL-delta?anchor+delta:0;
    }
    u64 delta=AP_GROUPED_CONNECT_IMAGE-image;
    return anchor>delta && anchor-delta>=0xffff800000000000ULL?anchor-delta:0;
}
static u64 grouped_member_address(unsigned cookie,u64 anchor) {
    switch(cookie) {
#define AP_MEMBER_ADDRESS(cookie,symbol,address) case cookie:return grouped_runtime_address(anchor,address);
    AP_MEMBERSHIP_SITES(AP_MEMBER_ADDRESS)
#undef AP_MEMBER_ADDRESS
    default:return 0;
    }
}
#ifdef AP_FTRACE_PROVIDER
static u64 grouped_session_address(u64 cookie,u64 anchor) {
    u64 image=0;
    switch(cookie) {
    case AP_ACCEPT_ENTRY_COOKIE:image=0xffffffff82197db0ULL;break;
    case AP_CONNECT_SECURITY_COOKIE:image=0xffffffff8206c070ULL;break;
    case AP_CONNECT_AUDIT_COOKIE:image=0xffffffff813f0fb0ULL;break;
    case AP_CONNECT_ENTRY_COOKIE:image=0xffffffff8206bf70ULL;break;
    case AP_FDUPFD_COOKIE:image=0xffffffff821ba4b0ULL;break;
    case AP_FDUPFD_ALLOC_COOKIE:image=0xffffffff821ba580ULL;break;
    case AP_EPOLL_CTL_COOKIE:image=0xffffffff81fb1ca0ULL;break;
    case AP_SHARED_FDGET_COOKIE:image=0xffffffff81fb0190ULL;break;
    case AP_STREAM_TX_COOKIE:image=AP_STREAM_TX_IMAGE;break;
    case AP_STREAM_TX_LOCK_COOKIE:image=AP_STREAM_TX_LOCK_IMAGE;break;
    case AP_STREAM_TX_UNLOCK_COOKIE:image=AP_STREAM_TX_UNLOCK_IMAGE;break;
    case AP_FILE_FDGET_COOKIE:image=0xffffffff81faaca0ULL;break;
    case AP_READ_FDGET_COOKIE:image=0xffffffff81faede0ULL;break;
    default:return 0;
    }
    return grouped_runtime_address(anchor,image);
}
#endif
#ifndef AP_FTRACE_PROVIDER
static struct bpf_link *grouped_attach(struct ap_session *s,const struct bpf_program *p) {
    if(!s->group_owner || !s->group_io || s->group_owner->phase!=AP_GROUPED_LEAVES ||
       !s->group_owner->event_id) {unavailable();return NULL;}
    /* Same CPU-wide perf context as the existing mode3 classic loader. The
     * event's ID came from the authenticated exact owned manager leaf. */
    struct perf_event_attr attr={.type=PERF_TYPE_TRACEPOINT,.size=sizeof(attr),
        .config=s->group_owner->event_id,.sample_period=1,.wakeup_events=1};
    int fd=(int)syscall(SYS_perf_event_open,&attr,-1,0,-1,PERF_FLAG_FD_CLOEXEC);
    if(fd<0)return NULL;
    struct ap_perf_event_opts options={.sz=sizeof(options),.bpf_cookie=AP_GROUPED_COOKIE};
    struct bpf_link *link=bpf_program__attach_perf_event_opts(p,fd,&options);
    long error=libbpf_get_error(link);
    if(!link || error) {
        int saved=error?(int)-error:errno;
        /* Installed libbpf transfers this perf FD only on successful attach.
         * Its failing attach paths free their wrapper but leave input FD. */
        int closed=close(fd);if(closed)return NULL;errno=saved;return NULL;
    }
    /* A real kernel BPF link is mandatory. The cookie forbids legacy ioctl
     * fallback; fresh link metadata below independently checks its type. */
    return link;
}
#endif
#ifdef AP_FTRACE_PROVIDER
static int grouped_observer_ready(struct ap_session *s) {
    if(!s->group_anchor || s->read_entry_program<0 || s->read_entry_link<0)return unavailable();
    struct bpf_prog_info program={0};struct bpf_link_info link={0};
    u32 ps=sizeof(program),ls=sizeof(link);
    if(bpf_obj_get_info_by_fd(s->read_entry_program,&program,&ps) ||
       bpf_obj_get_info_by_fd(s->read_entry_link,&link,&ls))return -1;
    if(!ap_ftrace_read_link_matches(&program,ps,&link,ls))return unavailable();
    unsigned matches=0;
    for(u32 i=0;i<s->links_count;i++) {
        struct bpf_link_info owned={0};u32 size=sizeof(owned);
        if(bpf_obj_get_info_by_fd(bpf_link__fd(s->links[i]),&owned,&size))return -1;
        if(size<offsetof(struct bpf_link_info,prog_id)+sizeof(owned.prog_id) ||
           !ap_ftrace_link_type_allowed(owned.type,BPF_LINK_TYPE_PERF_EVENT))return unavailable();
        if(owned.id==link.id) {
            if(owned.type!=link.type || owned.prog_id!=program.id)return unavailable();
            matches++;
        }
    }
    return matches==1?0:unavailable();
}
#else
static int grouped_observer_ready(struct ap_session *s) {
    if(!s->group_io || !s->group_owner || !s->group_anchor ||
       s->selection_program[1]<0 || s->selection_link[1]<0)return unavailable();
    struct bpf_prog_info program={0};struct bpf_link_info link={0};char symbol[64]={0};
    link.perf_event.kprobe.func_name=(u64)(uintptr_t)symbol;
    link.perf_event.kprobe.name_len=sizeof(symbol);
    u32 ps=sizeof(program),ls=sizeof(link);
    if(bpf_obj_get_info_by_fd(s->selection_program[1],&program,&ps) ||
       bpf_obj_get_info_by_fd(s->selection_link[1],&link,&ls))return -1;
    /* The installed dynamic-event info path names the first definition
     * returned by find_trace_kprobe. Full17 identity/misses are checked below;
     * this one projection never substitutes for that complete census. */
    if(ps<offsetof(struct bpf_prog_info,recursion_misses)+sizeof(program.recursion_misses) ||
       ls<offsetof(struct bpf_link_info,perf_event.kprobe.cookie)+sizeof(link.perf_event.kprobe.cookie) ||
       program.type!=BPF_PROG_TYPE_KPROBE || !program.id || program.recursion_misses ||
       link.type!=BPF_LINK_TYPE_PERF_EVENT || !link.id || link.prog_id!=program.id ||
       link.perf_event.type!=BPF_PERF_EVENT_KPROBE || link.perf_event.kprobe.cookie!=AP_GROUPED_COOKIE ||
       link.perf_event.kprobe.missed || link.perf_event.kprobe.offset!=AP_CONNECT_FDGET_RETURN_OFFSET ||
       link.perf_event.kprobe.name_len!=sizeof(AP_CONNECT_SYMBOL) ||
       memcmp(symbol,AP_CONNECT_SYMBOL,sizeof(AP_CONNECT_SYMBOL)))return unavailable();
    unsigned matches=0;
    for(u32 i=0;i<s->links_count;i++)if(s->link_identity[i].id==link.id) {
        if(s->link_identity[i].type!=link.type || s->link_identity[i].program_id!=program.id)return unavailable();
        matches++;
    }
    if(matches!=1)return unavailable();
    u64 deadline;if(grouped_deadline(&deadline))return -1;
    return ap_grouped_io_health(s->group_io,s->group_owner,deadline);
}
#endif
#ifdef AP_FTRACE_PROVIDER
static int grouped_retirement_ready(struct ap_session *s) {
    const u64 images[]={AP_FILE_RETIRE_IMAGE,AP_EXEC_CLOSE_IMAGE};
    for(unsigned which=0;which<2;which++) {
        int fd=s->retirement_program[which];u32 at=s->retirement_link[which];
        if(fd<0 || at>=s->links_count || !s->links[at])return unavailable();
        u64 address=0,cookie=~0ULL;
        struct bpf_prog_info program={0};struct bpf_link_info link={0};
        link.kprobe_multi.addrs=(u64)(uintptr_t)&address;
        link.kprobe_multi.cookies=(u64)(uintptr_t)&cookie;
        link.kprobe_multi.count=1;
        u32 ps=sizeof(program),ls=sizeof(link);
        if(bpf_obj_get_info_by_fd(fd,&program,&ps) ||
           bpf_obj_get_info_by_fd(bpf_link__fd(s->links[at]),&link,&ls))return -1;
        const struct ap_link_identity *bound=&s->link_identity[at];
        if(!ap_retirement_link_matches(which,grouped_runtime_address(s->group_anchor,images[which]),
              &program,ps,&link,ls,address,cookie) || program.id!=bound->program_id ||
           link.type!=bound->type || link.id!=bound->id)return unavailable();
    }
    return 0;
}
#endif
static int grouped_stream_ready(struct ap_session *s) {
    if(grouped_observer_ready(s))return -1;
#ifdef AP_FTRACE_PROVIDER
    /* Both new objects remain in the complete owned inventory and are queried
     * again at every copy drain. Observer-induced recursion is disqualifying,
     * never treated as evidence that no terminal fault happened. */
    for(unsigned which=0;which<2;which++) {
        const int fd=s->fault_program[which];const u32 at=s->fault_link[which];
        if(fd<0 || at>=s->links_count || !s->links[at])return unavailable();
        struct bpf_prog_info program={0};struct bpf_link_info link={0};
        u32 ps=sizeof(program),ls=sizeof(link);
        if(bpf_obj_get_info_by_fd(fd,&program,&ps) ||
           bpf_obj_get_info_by_fd(bpf_link__fd(s->links[at]),&link,&ls))return -1;
        const struct ap_link_identity *bound=&s->link_identity[at];
        if(ps<offsetof(struct bpf_prog_info,recursion_misses)+sizeof(program.recursion_misses) ||
           ls<offsetof(struct bpf_link_info,tracing.cookie)+sizeof(link.tracing.cookie) ||
           program.type!=BPF_PROG_TYPE_TRACING || !program.id || program.recursion_misses ||
           program.attach_btf_id!=AP_STREAM_FAULT_BTF_ID ||
           program.id!=bound->program_id || link.id!=bound->id || link.type!=bound->type ||
           link.type!=BPF_LINK_TYPE_TRACING || link.prog_id!=program.id ||
           link.tracing.target_obj_id!=program.attach_btf_obj_id ||
           link.tracing.target_btf_id!=AP_STREAM_FAULT_BTF_ID || link.tracing.cookie ||
           link.tracing.attach_type!=(which?BPF_TRACE_FEXIT:BPF_TRACE_FENTRY))return unavailable();
    }
#endif
    for(unsigned which=0;which<2;which++) {
        int fd=s->stream_program[which];u32 at=s->stream_link[which];
        if(fd<0 || at>=s->links_count || !s->links[at])return unavailable();
        u64 addresses[AP_MEMBERSHIP_ENTRY_COUNT]={0},cookies[AP_MEMBERSHIP_ENTRY_COUNT]={0};
        const unsigned count=which?AP_MEMBERSHIP_RETURN_COUNT:AP_MEMBERSHIP_ENTRY_COUNT;
        struct bpf_prog_info program={0};struct bpf_link_info link={0};
        link.kprobe_multi.addrs=(u64)(uintptr_t)addresses;link.kprobe_multi.cookies=(u64)(uintptr_t)cookies;
        link.kprobe_multi.count=count;u32 ps=sizeof(program),ls=sizeof(link);
        if(bpf_obj_get_info_by_fd(fd,&program,&ps) || bpf_obj_get_info_by_fd(bpf_link__fd(s->links[at]),&link,&ls))return -1;
        const struct ap_link_identity *bound=&s->link_identity[at];
#ifdef AP_FTRACE_PROVIDER
        u64 expected_addresses[AP_MEMBERSHIP_ENTRY_COUNT]={0};
        u64 expected_cookies[AP_MEMBERSHIP_ENTRY_COUNT]={0};
        for(unsigned i=0;i<count;i++) {
            expected_cookies[i]=i+1;
            expected_addresses[i]=grouped_member_address(i+1,s->group_anchor);
        }
        if(program.id!=bound->program_id || link.id!=bound->id || link.type!=bound->type ||
           !ap_ftrace_kprobe_multi_link_matches(&program,ps,&link,ls,addresses,cookies,
               expected_addresses,expected_cookies,count,
               which?BPF_F_KPROBE_MULTI_RETURN:0))return unavailable();
#else
        if(ps<offsetof(struct bpf_prog_info,recursion_misses)+sizeof(program.recursion_misses) ||
           ls<offsetof(struct bpf_link_info,kprobe_multi.cookies)+sizeof(link.kprobe_multi.cookies) ||
           program.type!=BPF_PROG_TYPE_KPROBE || !program.id || program.id!=bound->program_id ||
           program.recursion_misses || link.id!=bound->id || link.type!=bound->type ||
           link.type!=BPF_LINK_TYPE_KPROBE_MULTI || link.prog_id!=program.id ||
           link.kprobe_multi.count!=count || link.kprobe_multi.missed ||
           link.kprobe_multi.flags!=(which?BPF_F_KPROBE_MULTI_RETURN:0))return unavailable();
        u32 seen=0;
        for(unsigned i=0;i<count;i++) {
            if(cookies[i]<1 || cookies[i]>count || (seen&(1U<<cookies[i])) ||
               !addresses[i] || addresses[i]!=grouped_member_address((unsigned)cookies[i],s->group_anchor))return unavailable();
            seen|=1U<<cookies[i];
        }
        if(seen!=((1U<<(count+1))-2))return unavailable();
#endif
    }
    return 0;
}
static int grouped_anchor(struct ap_session *s,int config) {
    /* Explicit single-thread private loader, before any guest command. The
     * held self pidfd remains live until exact BPF receipt and activation. */
    pid_t pid=getpid(),tid=(pid_t)syscall(SYS_gettid);
    if(pid<=0 || tid!=pid)return unavailable();
    int owner=(int)syscall(SYS_pidfd_open,pid,0);if(owner<0)return -1;
    u32 zero=0;struct ap_config c={.provider=s->incarnation,
        .anchor_task=((u64)(u32)pid<<32)|(u32)tid,.anchor_phase=AP_GROUPED_ANCHOR_ARMED};
    int failed=bpf_map_update_elem(config,&zero,&c,BPF_ANY),saved=errno;
    if(!failed) {
        long raw=syscall(SYS_connect,-1,NULL,0);saved=errno;
        if(raw!=-1 || saved!=EBADF)failed=unavailable();
    }
    if(!failed) {
        struct ap_config observed={0};struct pollfd held={.fd=owner,.events=POLLIN};
        if(bpf_map_lookup_elem(config,&zero,&observed) || poll(&held,1,0)!=0 || held.revents ||
           observed.provider!=c.provider || observed.anchor_task!=c.anchor_task ||
           observed.anchor_phase!=AP_GROUPED_ANCHORED || !observed.anchor_start ||
           !ap_grouped_site_ip(1,observed.anchor_ip) ||
           ap_stream_fragment_source(observed.vmemmap_base,0,1,observed.vmemmap_base,
               observed.page_offset_base)!=observed.page_offset_base)failed=unavailable();
        else {
            s->group_anchor=observed.anchor_ip;observed.anchor_phase=AP_GROUPED_ANCHOR_ACTIVE;
            if(bpf_map_update_elem(config,&zero,&observed,BPF_ANY))failed=-1;
            else {struct ap_config actual={0};if(bpf_map_lookup_elem(config,&zero,&actual) ||
                memcmp(&actual,&observed,sizeof(actual)))failed=unavailable();}
        }
        saved=errno;
    }
    if(close(owner))return -1;
    errno=saved;return failed;
}
