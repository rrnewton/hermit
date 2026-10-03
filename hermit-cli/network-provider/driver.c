// Read-only BPF provider. All socket descriptors are borrowed, never closed here.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sys/syscall.h>
#include <linux/bpf.h>
#include <poll.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
#ifdef AP_FTRACE_PROVIDER
#include <linux/btf.h>
#endif
#include "provider.h"
#include "fd-effects.h"
#include "executable-source.h"
#include "connect-copy.h"
#include "retirement-target.h"
#include "task-disarm.h"
#include "stream-copy.h"
/* The branch is selected by driver-grouped.c, never by runtime metadata. */
u64 ap_provider_topology_version(void) {
#ifdef AP_FTRACE_PROVIDER
    return 2;
#elif defined(AP_GROUPED_PROVIDER)
    return 1;
#else
    return 0;
#endif
}
struct bpf_object;
struct bpf_program;
struct bpf_link;
struct bpf_map;
struct ring_buffer;
struct ring;
#ifdef AP_FTRACE_PROVIDER
struct btf;
extern struct btf *btf__load_vmlinux_btf(void);
extern int btf__find_by_name_kind(const struct btf *,const char *,__u32);
extern const struct btf_type *btf__type_by_id(const struct btf *,__u32);
extern void btf__free(struct btf *);
#endif
extern struct ring *ring_buffer__ring(struct ring_buffer *,unsigned int);
extern unsigned long ring__consumer_pos(const struct ring *);
extern unsigned long ring__producer_pos(const struct ring *);
extern struct ring_buffer *ring_buffer__new(int,int (*)(void *,void *,size_t),void *,const void *);
extern int ring_buffer__consume(struct ring_buffer *);
extern int ring_buffer__epoll_fd(const struct ring_buffer *);
extern void ring_buffer__free(struct ring_buffer *);
extern struct bpf_object *bpf_object__open_file(const char *,const void *);
extern long libbpf_get_error(const void *);
extern int bpf_object__load(struct bpf_object *);
extern void bpf_object__close(struct bpf_object *);
extern struct bpf_program *bpf_object__next_program(const struct bpf_object *,struct bpf_program *);
extern struct bpf_map *bpf_object__next_map(const struct bpf_object *,const struct bpf_map *);
extern struct bpf_link *bpf_program__attach(const struct bpf_program *);
extern const char *bpf_program__name(const struct bpf_program *);
extern struct bpf_link *bpf_program__attach_kprobe_multi_opts(const struct bpf_program *,const char *,const struct ap_kprobe_multi_opts *);
extern struct bpf_link *bpf_program__attach_kprobe_opts(const struct bpf_program *,const char *,const struct ap_kprobe_opts *);
extern int bpf_program__fd(const struct bpf_program *);
extern void bpf_program__unload(struct bpf_program *);
extern int bpf_link__fd(const struct bpf_link *);
extern int bpf_link__destroy(struct bpf_link *);
extern int bpf_map__fd(const struct bpf_map *);
extern int bpf_object__find_map_fd_by_name(const struct bpf_object *,const char *);
extern int bpf_map_update_elem(int,const void *,const void *,unsigned long long);
extern int bpf_map_lookup_elem(int,const void *,void *);
extern int bpf_map_delete_elem(int,const void *);
extern int bpf_obj_get_info_by_fd(int,void *,unsigned int *);
#ifdef AP_FTRACE_PROVIDER
#include "stream-copy-fault.h"
#define AP_PROGRAMS 49
#define AP_LINKS 49
#elif defined(AP_GROUPED_PROVIDER)
#include "grouped-io.h"
#define AP_PROGRAMS 44
#define AP_LINKS 44
#else
#define AP_PROGRAMS 46
#define AP_LINKS 58
#endif
#define AP_SHARED_SELECTION_LINKS 10
_Static_assert(AP_READ_SYSCALL==SYS_read,"bound scalar Read syscall ABI");
_Static_assert(AP_FILE_GET_FLAGS==F_GETFL && AP_FILE_SYSCALL_FCNTL==SYS_fcntl,"bound F_GETFL syscall ABI");
enum ap_slot_state { AP_SLOT_FREE, AP_SLOT_RESERVED, AP_SLOT_ACTIVE, AP_SLOT_DISARMING, AP_SLOT_COLLECTED, AP_SLOT_QUARANTINED };

struct ap_pending_command {
    struct ap_stream_copy_owned stream_copy;
    union { struct ap_stream_tx_owned stream_tx;
        struct ap_stream_tx_blocking_owned stream_tx_blocking; };
    enum ap_slot_state state;
    struct ap_task_command submitted;
    struct ap_command_result receipt;
    struct ap_task_disarm_outcome disarm;
    struct ap_fd_accept fd_receipt;
    bool fd_collected;
    struct ap_executable_source executable_receipt;
    bool executable_collected;
    struct ap_fd_enrollment enrollment_receipt;
    bool enrollment_collected;
    struct ap_original_selection original_selection;
    struct ap_fd_call original_receipt;
    bool original_selected,original_collected,epoll_collected;
    struct ap_native_birth birth_receipt;
    struct ap_fd_call birth_completed;
    bool birth_observed,birth_child_admitted,birth_child_terminal,birth_collected;
};
static int executable_reserve(struct ap_session *,const struct ap_task_command *,const struct ap_executable_intent *);
static int executable_ack(struct ap_session *,struct ap_pending_command *);
static int fd_reserve_accept(struct ap_session *,const struct ap_task_command *);
static int fd_ack_accept(struct ap_session *,struct ap_pending_command *);
static int fd_reserve_enrollment(struct ap_session *,const struct ap_task_command *);
static int fd_ack_enrollment(struct ap_session *,struct ap_pending_command *);
static int fd_ack_original(struct ap_session *,struct ap_pending_command *);
static int fd_ack_epoll_ctl_copy(struct ap_session *,struct ap_pending_command *);
static int fd_ack_birth(struct ap_session *,struct ap_pending_command *);
struct ap_link_identity { u32 type,id,program_id,program_type; };
struct ap_session {
#ifdef AP_GROUPED_PROVIDER
    struct ap_grouped_io *group_io;
    struct ap_grouped_owner *group_owner;
    u64 group_anchor;
#endif
    struct bpf_object *object;
    struct ring_buffer *stream_copy_ring;
    int stream_copy_error;
    struct bpf_link *links[AP_LINKS];
    struct ap_link_identity link_identity[AP_LINKS];
    u32 links_count;
    int tasks,commands,events,status;
    int fdget_program,fdget_link;
#ifdef AP_FTRACE_PROVIDER
    int fdget_extra_program,fdget_extra_link;
    int fdget_shared_program[2],fdget_shared_link[2];
#endif
    int read_entry_program,read_entry_link;
#ifdef AP_FTRACE_PROVIDER
    int retirement_program[2];
    u32 retirement_link[2];
    int fault_program[2];
    u32 fault_link[2];
#endif
    int copy_program[2],copy_link[2];
    int stream_program[6];u32 stream_link[6];
    int selection_program[9],selection_link[9];
    u64 incarnation,next_command;
    bool ready;
    atomic_flag command_busy; /* Nonblocking serialization of submit/read/ACK. */
    struct ap_pending_command pending[AP_COMMANDS];
};
static int stream_copy_record(void *,void *,size_t);
static int stream_copy_drain(struct ap_session *);
static int fd_thread_exited(int,bool *);
static int fd_require_dead_thread(int);
static int unavailable(void) { errno=ENODATA; return -1; }
static int invalid(void) { errno=EINVAL; return -1; }
#ifdef AP_FTRACE_PROVIDER
/* Refusal diagnostics use only the query which failed the existing predicate.
 * They do not requery, retry, repair or authorize a missing observation. */
static int observer_metadata_unavailable(const char *stage,unsigned at,
        const struct bpf_prog_info *program,unsigned ps,
        const struct bpf_link_info *link,unsigned ls) {
    fprintf(stderr,"accepted observer refusal stage=%s index=%u program_size=%u "
        "program_type=%u program_id=%u recursion_misses=%llu link_size=%u "
        "link_type=%u link_id=%u link_program_id=%u multi_count=%u multi_flags=%u "
        "multi_missed=%llu\n",stage,at,ps,program->type,program->id,
        (unsigned long long)program->recursion_misses,ls,link->type,link->id,
        link->prog_id,link->type==BPF_LINK_TYPE_KPROBE_MULTI?link->kprobe_multi.count:0,
        link->type==BPF_LINK_TYPE_KPROBE_MULTI?link->kprobe_multi.flags:0,
        link->type==BPF_LINK_TYPE_KPROBE_MULTI?(unsigned long long)link->kprobe_multi.missed:0);
    return unavailable();
}
/* ap_require_grouped_target has already authenticated the exact GNU build ID,
 * kallsyms coordinates and retained image bytes. Independently read the live
 * page size and vmlinux BTF struct-page size before accepting the direct-map
 * fragment translation; neither value is inferred from that identity. */
static int ap_require_stream_fragment_image(void) {
    long page_size=sysconf(_SC_PAGESIZE);
    if(page_size<=0)return -1;
    struct btf *btf=btf__load_vmlinux_btf();long error=libbpf_get_error(btf);
    if(!btf || error) {if(error)errno=(int)-error;return -1;}
    int id=btf__find_by_name_kind(btf,"page",BTF_KIND_STRUCT);
    const struct btf_type *page=id>0?btf__type_by_id(btf,(__u32)id):NULL;
    u64 page_struct_size=page && BTF_INFO_KIND(page->info)==BTF_KIND_STRUCT?page->size:0;
    btf__free(btf);
    if(id<=0 || !page) {errno=id<0?-id:ENODATA;return -1;}
    if(!ap_stream_fragment_image_layout((u64)page_size,page_struct_size)) {
        errno=ESTALE;return -1;
    }
    return 0;
}
#endif
static int enter_commands(struct ap_session *s) {
    if(!s || !s->ready)return invalid();
    if(atomic_flag_test_and_set_explicit(&s->command_busy,memory_order_acquire)) { errno=EBUSY; return -1; }
    return 0;
}
static void leave_commands(struct ap_session *s) {
    atomic_flag_clear_explicit(&s->command_busy,memory_order_release);
}
static int quarantine(struct ap_pending_command *p) {
    p->state=AP_SLOT_QUARANTINED;return -1;
}
#ifdef AP_FTRACE_PROVIDER
#include "grouped-driver.h"
#define AP_ACTIVE_CONNECT_SESSION_SITES AP_CONNECT_SESSION_SITES_FTRACE
#define AP_ACTIVE_CONNECT_SESSION_COOKIE_MASK AP_CONNECT_SESSION_COOKIE_MASK_FTRACE
#elif defined(AP_GROUPED_PROVIDER)
#include "grouped-driver.h"
#define AP_ACTIVE_CONNECT_SESSION_SITES AP_CONNECT_SESSION_SITES
#define AP_ACTIVE_CONNECT_SESSION_COOKIE_MASK AP_CONNECT_SESSION_COOKIE_MASK
#else
#define AP_ACTIVE_CONNECT_SESSION_SITES AP_CONNECT_SESSION_SITES
#define AP_ACTIVE_CONNECT_SESSION_COOKIE_MASK AP_CONNECT_SESSION_COOKIE_MASK
#endif
/* Kernel-issued link info is checked at admission and collection. Counters
 * only disqualify; zero does not replace the mandatory before/after receipts. */
static int fd_classic_observer_ready(struct ap_session *s,bool selection) {
#ifdef AP_GROUPED_PROVIDER
    (void)selection;return grouped_observer_ready(s);
#else
    for(unsigned which=0;which<(selection?9:2);which++) {
        int program_fd=selection?s->selection_program[which]:s->copy_program[which];
        int link_fd=selection?s->selection_link[which]:s->copy_link[which];
        if(program_fd<0 || link_fd<0)return unavailable();
        struct bpf_prog_info program={0};struct bpf_link_info link={0};
        char symbol[64]={0};
        link.perf_event.kprobe.func_name=(u64)(uintptr_t)symbol;
        link.perf_event.kprobe.name_len=sizeof(symbol);
        unsigned int ps=sizeof(program),ls=sizeof(link);
        if(bpf_obj_get_info_by_fd(program_fd,&program,&ps) ||
           bpf_obj_get_info_by_fd(link_fd,&link,&ls))return -1;
        if(!(selection?ap_fdget_link_matches(which,&program,ps,&link,ls,symbol):
             ap_copy_link_matches(which,&program,ps,&link,ls,symbol)))return unavailable();
    }
    return 0;
#endif
}
/* Diagnostics can disqualify a receipt; zero counters never establish that an
 * unobserved invocation did not run. In particular, fgraph recursion has a
 * documented uncounted-miss path. The actual completed selection is mandatory. */
static int fd_accept_observer_ready_mode(struct ap_session *s,bool task_scoped_receipt) {
    if(s->fdget_program<0 || s->fdget_link<0)return unavailable();
#ifdef AP_FTRACE_PROVIDER
    if(s->fdget_extra_program<0 || s->fdget_extra_link<0)return unavailable();
    for(unsigned i=0;i<2;i++)
        if(s->fdget_shared_program[i]<0 || s->fdget_shared_link[i]<0)return unavailable();
    static const u64 expected_cookies[2][5]={
        {AP_ACCEPT_ENTRY_COOKIE,AP_CONNECT_ENTRY_COOKIE,AP_FDUPFD_COOKIE,AP_EPOLL_CTL_COOKIE},
        {AP_CONNECT_SECURITY_COOKIE,AP_CONNECT_AUDIT_COOKIE,AP_FDUPFD_ALLOC_COOKIE,
         AP_FILE_FDGET_COOKIE,AP_READ_FDGET_COOKIE}
    };
    for(unsigned group=0;group<2;group++) {
        struct bpf_prog_info program={0};struct bpf_link_info link={0};
        u64 addresses[5]={0},cookies[5]={0},expected_addresses[5]={0};
        unsigned count=group?5:4;
        link.kprobe_multi.addrs=(u64)(uintptr_t)addresses;
        link.kprobe_multi.count=count;link.kprobe_multi.cookies=(u64)(uintptr_t)cookies;
        unsigned program_size=sizeof(program),link_size=sizeof(link);
        int program_fd=group?s->fdget_extra_program:s->fdget_program;
        int link_fd=group?s->fdget_extra_link:s->fdget_link;
        if(bpf_obj_get_info_by_fd(program_fd,&program,&program_size) ||
           bpf_obj_get_info_by_fd(link_fd,&link,&link_size))return -1;
        for(unsigned n=0;n<count;n++)
            expected_addresses[n]=grouped_session_address(expected_cookies[group][n],s->group_anchor);
        if(!ap_ftrace_kprobe_multi_link_matches(&program,program_size,&link,link_size,
              addresses,cookies,expected_addresses,expected_cookies[group],count,0))
            return observer_metadata_unavailable("selection",group,
                &program,program_size,&link,link_size);
    }
    for(unsigned which=0;which<2;which++) {
        struct bpf_prog_info program={0};struct bpf_link_info link={0};
        u64 addresses[4]={0},cookies[4]={0};
        const u64 expected_addresses[4]={grouped_session_address(AP_SHARED_FDGET_COOKIE,s->group_anchor),
            grouped_session_address(AP_STREAM_TX_COOKIE,s->group_anchor),
            grouped_session_address(AP_STREAM_TX_LOCK_COOKIE,s->group_anchor),
            grouped_session_address(AP_STREAM_TX_UNLOCK_COOKIE,s->group_anchor)};
        const u64 expected_cookies[4]={AP_SHARED_FDGET_COOKIE,AP_STREAM_TX_COOKIE,
            AP_STREAM_TX_LOCK_COOKIE,AP_STREAM_TX_UNLOCK_COOKIE};
        link.kprobe_multi.addrs=(u64)(uintptr_t)addresses;link.kprobe_multi.count=4;
        link.kprobe_multi.cookies=(u64)(uintptr_t)cookies;
        unsigned ps=sizeof(program),ls=sizeof(link);
        if(bpf_obj_get_info_by_fd(s->fdget_shared_program[which],&program,&ps) ||
           bpf_obj_get_info_by_fd(s->fdget_shared_link[which],&link,&ls))return -1;
        int matches=task_scoped_receipt?
            ap_ftrace_task_scoped_kprobe_multi_link_matches(&program,ps,&link,ls,
                addresses,cookies,expected_addresses,expected_cookies,4,
                which?BPF_F_KPROBE_MULTI_RETURN:0):
            ap_ftrace_kprobe_multi_link_matches(&program,ps,&link,ls,
                addresses,cookies,expected_addresses,expected_cookies,4,
                which?BPF_F_KPROBE_MULTI_RETURN:0);
        if(!matches)return observer_metadata_unavailable("shared-selection",which,
            &program,ps,&link,ls);
    }
#else
    (void)task_scoped_receipt;
    struct bpf_prog_info program={0};struct bpf_link_info link={0};
    u64 addresses[AP_ACTIVE_CONNECT_SESSION_SITES]={0},cookies[AP_ACTIVE_CONNECT_SESSION_SITES]={0};
    /* The kernel requires addrs together with a nonzero count, even for a
     * cookie query. Addresses may be masked and are not identity evidence. */
    link.kprobe_multi.addrs=(u64)(uintptr_t)addresses;
    link.kprobe_multi.count=AP_ACTIVE_CONNECT_SESSION_SITES;link.kprobe_multi.cookies=(u64)(uintptr_t)cookies;
    unsigned int program_size=sizeof(program),link_size=sizeof(link);
    if(bpf_obj_get_info_by_fd(s->fdget_program,&program,&program_size) ||
       bpf_obj_get_info_by_fd(s->fdget_link,&link,&link_size))return -1;
    if(program_size<offsetof(struct bpf_prog_info,recursion_misses)+sizeof(program.recursion_misses) ||
       link_size<offsetof(struct bpf_link_info,kprobe_multi.cookies)+sizeof(link.kprobe_multi.cookies) ||
       program.type!=BPF_PROG_TYPE_KPROBE || !program.id || !link.id ||
       link.type!=BPF_LINK_TYPE_KPROBE_MULTI || link.prog_id!=program.id ||
       link.kprobe_multi.count!=AP_ACTIVE_CONNECT_SESSION_SITES || link.kprobe_multi.flags ||
       program.recursion_misses || link.kprobe_multi.missed)return unavailable();
    u64 seen=0;
    for(unsigned n=0;n<AP_ACTIVE_CONNECT_SESSION_SITES;n++) {
        if(cookies[n]>=64 || !(AP_ACTIVE_CONNECT_SESSION_COOKIE_MASK&(1ULL<<cookies[n])) || (seen&(1ULL<<cookies[n])))
            return unavailable();
#ifdef AP_FTRACE_PROVIDER
        if(!addresses[n] || addresses[n]!=grouped_session_address(cookies[n],s->group_anchor))
            return unavailable();
#endif
        seen|=1ULL<<cookies[n];
    }
    if(seen!=AP_ACTIVE_CONNECT_SESSION_COOKIE_MASK)return unavailable();
#endif
    if(fd_classic_observer_ready(s,false))return -1;
    return fd_classic_observer_ready(s,true);
}
static int fd_accept_observer_ready(struct ap_session *s) {
    return fd_accept_observer_ready_mode(s,false);
}
static int fd_accept_observer_ready_runtime(struct ap_session *s) {
    return fd_accept_observer_ready_mode(s,true);
}
/* Query the owned link itself. Saved identity constrains a fresh query; it
 * never substitutes for a missing/short/failed kernel response. Unbound links
 * are possible during partial attachment; only earlier positively bound pairs
 * may already have released their redundant program FDs. */
static int owned_link_info(struct ap_session *s,u32 at,struct bpf_link_info *out) {
    if(at>=s->links_count || at>=AP_LINKS || !s->links[at])return unavailable();
    int fd=bpf_link__fd(s->links[at]);
    if(fd<0)return unavailable();
    memset(out,0,sizeof(*out));u32 size=sizeof(*out);
    if(bpf_obj_get_info_by_fd(fd,out,&size))return -1;
    if(size<offsetof(struct bpf_link_info,prog_id)+sizeof(out->prog_id) ||
       !out->type || !out->id || !out->prog_id)return unavailable();
    const struct ap_link_identity *expected=&s->link_identity[at];
    if((expected->program_id || s->ready) &&
       (out->type!=expected->type || out->id!=expected->id || out->prog_id!=expected->program_id))
        return unavailable();
    return 0;
}
/* Each original link binds its actual loaded program before that redundant
 * userspace load FD can go. Link custody survives both success and any later
 * partial-attach failure. The extra site shares only the retained Connect
 * selection program; every original program/link remains independently owned. */
static int bind_observer_link(struct ap_session *s,u32 at,const struct bpf_program *p,bool shared) {
    int fd=bpf_program__fd(p);if(fd<0)return unavailable();
    struct bpf_prog_info program={0};u32 size=sizeof(program);
    if(bpf_obj_get_info_by_fd(fd,&program,&size))return -1;
    if(size<offsetof(struct bpf_prog_info,id)+sizeof(program.id) || !program.type || !program.id)
        return unavailable();
    struct bpf_link_info link;if(owned_link_info(s,at,&link))return -1;
    if(link.prog_id!=program.id)return unavailable();
#ifdef AP_FTRACE_PROVIDER
    if(!ap_ftrace_program_link_pair_allowed(program.type,link.type,BPF_LINK_TYPE_PERF_EVENT))
        return unavailable();
#endif
    unsigned expected_prior=0;
    if(shared) {
        if(at<AP_PROGRAMS || at>=AP_LINKS)return unavailable();
        int expected=at<AP_PROGRAMS+AP_SHARED_SELECTION_LINKS?s->selection_program[1]:s->stream_program[at-(AP_PROGRAMS+AP_SHARED_SELECTION_LINKS)+2];
        if(fd!=expected)return unavailable();
        expected_prior=at<AP_PROGRAMS+AP_SHARED_SELECTION_LINKS?at-AP_PROGRAMS+1:1;
    }
    unsigned prior_programs=0;
    for(u32 i=0;i<at;i++) {
        if(s->link_identity[i].id==link.id)return unavailable();
        prior_programs+=s->link_identity[i].program_id==program.id;
    }
    if(prior_programs!=expected_prior)return unavailable();
    s->link_identity[at]=(struct ap_link_identity){link.type,link.id,program.id,program.type};return 0;
}
/* A completed copy sequence is mandatory evidence. Fresh loss counters only
 * disqualify it; zero misses cannot establish bytes, ordering or immutability. */
static int stream_copy_observer_ready(struct ap_session *s) {
#ifdef AP_GROUPED_PROVIDER
    return grouped_stream_ready(s);
#else
    for(unsigned which=0;which<6;which++) {
        int fd=s->stream_program[which];u32 at=s->stream_link[which];
        if(fd<0 || at>=s->links_count || !s->links[at])return unavailable();
        struct bpf_prog_info program={0};struct bpf_link_info link={0};
        u64 addresses[4]={0},cookies[4]={0};char symbol[64]={0};
        if(which<2) {
            link.kprobe_multi.addrs=(u64)(uintptr_t)addresses;
            link.kprobe_multi.cookies=(u64)(uintptr_t)cookies;link.kprobe_multi.count=4;
        } else {
            link.perf_event.kprobe.func_name=(u64)(uintptr_t)symbol;
            link.perf_event.kprobe.name_len=sizeof(symbol);
        }
        u32 ps=sizeof(program),ls=sizeof(link);
        if(bpf_obj_get_info_by_fd(fd,&program,&ps) ||
           bpf_obj_get_info_by_fd(bpf_link__fd(s->links[at]),&link,&ls))return -1;
        const struct ap_link_identity *bound=&s->link_identity[at];
        if(ps<offsetof(struct bpf_prog_info,recursion_misses)+sizeof(program.recursion_misses) ||
           ls<offsetof(struct bpf_link_info,prog_id)+sizeof(link.prog_id) ||
           !program.id || program.recursion_misses || program.id!=bound->program_id ||
           link.id!=bound->id || link.type!=bound->type || link.prog_id!=program.id)
            return unavailable();
        if(which<2) {
            if(program.type!=BPF_PROG_TYPE_KPROBE || link.type!=BPF_LINK_TYPE_KPROBE_MULTI ||
               ls<offsetof(struct bpf_link_info,kprobe_multi.cookies)+sizeof(link.kprobe_multi.cookies) ||
               link.kprobe_multi.count!=4 || link.kprobe_multi.missed ||
               link.kprobe_multi.flags!=(which?BPF_F_KPROBE_MULTI_RETURN:0))return unavailable();
            unsigned seen=0;
            for(unsigned i=0;i<4;i++) {
                if(cookies[i]<1 || cookies[i]>4 || (seen&(1U<<cookies[i])))return unavailable();
                seen|=1U<<cookies[i];
            }
            if(seen!=30)return unavailable();
        } else if(!ap_stream_copy_link_matches(which-2,&program,ps,&link,ls,symbol))
            return unavailable();
    }
    return 0;
#endif
}
static int finish_observer_load(struct ap_session *s) {
    if(s->links_count!=AP_LINKS)return unavailable();
    if(fd_accept_observer_ready(s) || stream_copy_observer_ready(s))return -1;
#ifdef AP_FTRACE_PROVIDER
    if(grouped_retirement_ready(s))return -1;
#endif
    struct bpf_program *p=NULL;u32 count=0;
    while((p=bpf_object__next_program(s->object,p))) {
        if(count==AP_PROGRAMS || !s->link_identity[count].program_id)return unavailable();
        struct bpf_link_info link;if(owned_link_info(s,count,&link))return -1;
        int fd=bpf_program__fd(p);
        if(fd>=0) {
            struct bpf_prog_info program={0};u32 size=sizeof(program);
            if(bpf_obj_get_info_by_fd(fd,&program,&size))return -1;
            if(size<offsetof(struct bpf_prog_info,id)+sizeof(program.id) ||
               !program.type || program.id!=link.prog_id)return unavailable();
        }
        for(u32 i=0;i<count;i++)
            if(s->link_identity[i].id==link.id || s->link_identity[i].program_id==link.prog_id)
                return unavailable();
        count++;
    }
    if(count!=AP_PROGRAMS)return unavailable();
#ifdef AP_FTRACE_PROVIDER
    if(s->read_entry_program<0 || s->read_entry_link<0)return unavailable();
#else
    if(s->copy_program[0]!=s->selection_program[1] ||
       s->copy_program[1]!=s->selection_program[1] ||
       s->stream_program[4]!=s->stream_program[2] || s->stream_program[5]!=s->stream_program[3])return unavailable();
    for(unsigned which=0;which<9;which++)
        if(s->selection_program[which]!=s->selection_program[1])return unavailable();
#endif
    for(u32 at=AP_PROGRAMS;at<AP_LINKS;at++) {
        struct bpf_link_info extra;if(owned_link_info(s,at,&extra))return -1;
        for(u32 i=0;i<at;i++)if(s->link_identity[i].id==extra.id)return unavailable();
    }
    s->ready=true;return 0;
}
#if defined(AP_GROUPED_PROVIDER) && !defined(AP_FTRACE_PROVIDER)
/* Legacy constructor cannot silently activate the new global-resource ABI. */
int ap_open(const char *path,u64 incarnation,struct ap_session **out) {
    (void)path;(void)incarnation;if(out)*out=NULL;errno=ENOTSUP;return -1;
}
static int open_grouped_body(const char *path,u64 incarnation,struct ap_grouped_io *io,
        struct ap_grouped_owner *owner,u64 deadline,struct ap_session **out);
int ap_open_grouped(const char *path,u64 incarnation,struct ap_grouped_io *io,
        struct ap_grouped_owner *owner,u64 deadline,struct ap_session **out) {
    return open_grouped_body(path,incarnation,io,owner,deadline,out);
}
static int open_grouped_body(const char *path,u64 incarnation,struct ap_grouped_io *io,
        struct ap_grouped_owner *owner,u64 deadline,struct ap_session **out) {
    if(!io || !owner || owner->incarnation!=incarnation || owner->phase!=AP_GROUPED_LEAVES ||
       ap_grouped_io_health(io,owner,deadline))return unavailable();
#else
int ap_open(const char *path,u64 incarnation,struct ap_session **out) {
#endif
    if(!path || !incarnation || !out)return invalid();
    *out=NULL;
#ifdef AP_GROUPED_PROVIDER
    if(ap_require_grouped_target()
#ifdef AP_FTRACE_PROVIDER
       || ap_require_stream_fragment_image()
#endif
      )return -1;
#else
    if(ap_require_retirement_target())return -1;
#endif
    struct ap_session *s=calloc(1,sizeof(*s));
    if(!s)return -1;
    *out=s; /* even partial load/attach failure returns owned cleanup inventory */
#if defined(AP_GROUPED_PROVIDER) && !defined(AP_FTRACE_PROVIDER)
    s->group_io=io;s->group_owner=owner;
#endif
    s->tasks=s->commands=s->events=s->status=-1;
    s->fdget_program=s->fdget_link=-1;
#ifdef AP_FTRACE_PROVIDER
    s->fdget_extra_program=s->fdget_extra_link=-1;
    for(unsigned i=0;i<2;i++)s->fdget_shared_program[i]=s->fdget_shared_link[i]=-1;
#endif
    s->read_entry_program=s->read_entry_link=-1;
#ifdef AP_FTRACE_PROVIDER
    for(unsigned i=0;i<2;i++) {s->retirement_program[i]=-1;s->retirement_link[i]=AP_LINKS;}
    for(unsigned i=0;i<2;i++) {s->fault_program[i]=-1;s->fault_link[i]=AP_LINKS;}
#endif
    for(unsigned i=0;i<6;i++) {s->stream_program[i]=-1;s->stream_link[i]=AP_LINKS;}
    for(unsigned i=0;i<2;i++) {
        s->copy_program[i]=s->copy_link[i]=-1;
    }
    for(unsigned i=0;i<9;i++)s->selection_program[i]=s->selection_link[i]=-1;
    s->incarnation=incarnation;
    atomic_flag_clear(&s->command_busy);
    s->object=bpf_object__open_file(path,NULL);
    long error=libbpf_get_error(s->object);
    if(!s->object || error) { s->object=NULL; if(error)errno=-error; return -1; }
    if(bpf_object__load(s->object))return -1;
    int config=bpf_object__find_map_fd_by_name(s->object,"ap_config_map");
    s->tasks=bpf_object__find_map_fd_by_name(s->object,"tasks");
    s->commands=bpf_object__find_map_fd_by_name(s->object,"commands");
    s->events=bpf_object__find_map_fd_by_name(s->object,"events");
    s->status=bpf_object__find_map_fd_by_name(s->object,"status");
    if(config<0 || s->tasks<0 || s->commands<0 || s->events<0 || s->status<0)return unavailable();
    /* ABI10 extends only the task command. Refuse an older or foreign map
     * before the first map write or any observer attachment. */
    struct bpf_map_info task_info={0};u32 task_size=sizeof(task_info);
    if(bpf_obj_get_info_by_fd(s->tasks,&task_info,&task_size) ||
       task_size<offsetof(struct bpf_map_info,map_flags)+sizeof(task_info.map_flags) ||
       !task_info.id || task_info.type!=BPF_MAP_TYPE_TASK_STORAGE ||
       task_info.key_size!=sizeof(int) || task_info.value_size!=sizeof(struct ap_task_command) ||
       task_info.max_entries || task_info.map_flags!=BPF_F_NO_PREALLOC)return unavailable();
#ifdef AP_FTRACE_PROVIDER
    int fault_map=bpf_object__find_map_fd_by_name(s->object,"stream_copy_faults");
    struct bpf_map_info fault_info={0};u32 fault_size=sizeof(fault_info);
    if(fault_map<0 || bpf_obj_get_info_by_fd(fault_map,&fault_info,&fault_size))return unavailable();
    if(fault_size<offsetof(struct bpf_map_info,max_entries)+sizeof(fault_info.max_entries) ||
       !fault_info.id || fault_info.type!=BPF_MAP_TYPE_ARRAY ||
       fault_info.key_size!=sizeof(u32) || fault_info.value_size!=sizeof(struct ap_stream_fault_state) ||
       fault_info.max_entries!=AP_COMMANDS)return unavailable();
#endif
    int exe_map=bpf_object__find_map_fd_by_name(s->object,"executable_sources");
    struct bpf_map_info exe_info={0};u32 exe_size=sizeof(exe_info);
    if(exe_map<0 || bpf_obj_get_info_by_fd(exe_map,&exe_info,&exe_size) ||
       exe_size<offsetof(struct bpf_map_info,map_flags)+sizeof(exe_info.map_flags) ||
       !exe_info.id || exe_info.type!=BPF_MAP_TYPE_ARRAY || exe_info.key_size!=sizeof(u32) ||
       exe_info.value_size!=sizeof(struct ap_executable_source) ||
       exe_info.max_entries!=AP_COMMANDS || exe_info.map_flags)return unavailable();
    u32 zero=0;struct ap_config c={.provider=incarnation};
    if(bpf_map_update_elem(config,&zero,&c,BPF_ANY))return -1;
    int copy_map=bpf_object__find_map_fd_by_name(s->object,"stream_copy_records");
    if(copy_map<0)return unavailable();
    s->stream_copy_ring=ring_buffer__new(copy_map,stream_copy_record,s,NULL);
    if(!s->stream_copy_ring)return -1;
    struct bpf_program *p=NULL;
#ifndef AP_FTRACE_PROVIDER
    struct bpf_program *selection_shared=NULL;
#endif
#ifndef AP_GROUPED_PROVIDER
    struct bpf_program *stream_shared[2]={NULL,NULL};
#endif
    while((p=bpf_object__next_program(s->object,p))) {
        if(s->links_count==AP_PROGRAMS) { errno=EOVERFLOW; return -1; }
        struct bpf_link *link;
#ifdef AP_FTRACE_PROVIDER
        bool fdget_outer=!strcmp(bpf_program__name(p),"fd_so");
        int fdget_group=!strcmp(bpf_program__name(p),"fd_si")?1:fdget_outer?0:-1;
        int fdget_shared=!strcmp(bpf_program__name(p),"fd_s20e")?0:
            !strcmp(bpf_program__name(p),"fd_s20x")?1:-1;
#else
        bool fdget_outer=!strcmp(bpf_program__name(p),"fd_accept_selected");
        int fdget_group=fdget_outer?0:-1;
        int fdget_shared=-1;
#endif
        bool fdget=fdget_group>=0;
        bool read_entry=!strcmp(bpf_program__name(p),"fd_original_read_entered");
#ifdef AP_FTRACE_PROVIDER
        int retirement=!strcmp(bpf_program__name(p),"fd_file_retired")?0:
            !strcmp(bpf_program__name(p),"fd_exec_closed_file")?1:-1;
        int fault=!strcmp(bpf_program__name(p),"fd_stream_fault_enter")?0:
            !strcmp(bpf_program__name(p),"fd_stream_fault_exit")?1:-1;
#endif
        int selection=!strcmp(bpf_program__name(p),"fd_connect_post_fdget")?1:-1;
        int stream=!strcmp(bpf_program__name(p),"fd_stream_copy_protocol_enter")?0:
            !strcmp(bpf_program__name(p),"fd_stream_copy_protocol_exit")?1:
            !strcmp(bpf_program__name(p),"fd_stream_copy_enter")?2:
            !strcmp(bpf_program__name(p),"fd_stream_copy_exit")?3:-1;
        if(stream==0 || stream==1) {
#ifdef AP_GROUPED_PROVIDER
            static const char *symbols[]={
#define AP_MEMBER_SYMBOL(cookie,symbol,address) #symbol,
                AP_MEMBERSHIP_SITES(AP_MEMBER_SYMBOL)
#undef AP_MEMBER_SYMBOL
            };
            static const unsigned long long cookies[]={
#define AP_MEMBER_COOKIE(cookie,symbol,address) cookie,
                AP_MEMBERSHIP_SITES(AP_MEMBER_COOKIE)
#undef AP_MEMBER_COOKIE
            };
            struct ap_kprobe_multi_opts options={.sz=sizeof(options),.syms=symbols,.cookies=cookies,
                .cnt=stream?AP_MEMBERSHIP_RETURN_COUNT:AP_MEMBERSHIP_ENTRY_COUNT,.retprobe=stream==1};
#else
            const char *symbols[]={"inet_recvmsg","inet6_recvmsg","unix_stream_recvmsg","skb_copy_datagram_iter"};
            const unsigned long long cookies[]={1,2,3,4};
            struct ap_kprobe_multi_opts options={.sz=sizeof(options),.syms=symbols,.cookies=cookies,.cnt=4,
                .retprobe=stream==1};
#endif
            link=bpf_program__attach_kprobe_multi_opts(p,NULL,&options);
        } else if(stream==2 || stream==3) {
#ifdef AP_FTRACE_PROVIDER
            /* A mismatched object must fail before opening any classic perf
             * event; the accepted ftrace artifact contains neither program. */
            return unavailable();
#else
            struct ap_kprobe_opts options={.sz=sizeof(options),
                .bpf_cookie=stream==2?AP_STREAM_COPY_ENTRY_COOKIE:AP_STREAM_COPY_EXIT_COOKIE,
                .offset=stream==2?AP_STREAM_COPY_ENTRY_OFFSET:AP_STREAM_COPY_EXIT_OFFSET,
                .retprobe=false,.attach_mode=3};
            link=bpf_program__attach_kprobe_opts(p,AP_STREAM_COPY_SYMBOL,&options);
#endif
        } else if(fdget || fdget_shared>=0 || !strcmp(bpf_program__name(p),"fd_file_retired") ||
           !strcmp(bpf_program__name(p),"fd_exec_closed_file")) {
            const char *symbols[]={fdget?AP_ACCEPT_SYMBOL:!strcmp(bpf_program__name(p),"fd_file_retired")?
                AP_FILE_RETIRE_SYMBOL:AP_EXEC_CLOSE_SYMBOL,AP_CONNECT_SECURITY_SYMBOL,
                AP_CONNECT_AUDIT_SYMBOL,AP_CONNECT_SYMBOL,AP_FDUPFD_SYMBOL,AP_ALLOC_FD_SYMBOL,AP_EPOLL_CTL_SYMBOL
#ifdef AP_FTRACE_PROVIDER
                ,AP_FDGET_SYMBOL,AP_FILE_SYMBOL,AP_READ_SELECTED_SYMBOL
#endif
                };
            const unsigned long long cookies[]={AP_ACCEPT_ENTRY_COOKIE,AP_CONNECT_SECURITY_COOKIE,
                AP_CONNECT_AUDIT_COOKIE,AP_CONNECT_ENTRY_COOKIE,AP_FDUPFD_COOKIE,AP_FDUPFD_ALLOC_COOKIE,AP_EPOLL_CTL_COOKIE
#ifdef AP_FTRACE_PROVIDER
                ,AP_SHARED_FDGET_COOKIE,AP_FILE_FDGET_COOKIE,AP_READ_FDGET_COOKIE
#endif
                };
#ifdef AP_FTRACE_PROVIDER
            static const char *outer_symbols[]={AP_ACCEPT_SYMBOL,AP_CONNECT_SYMBOL,
                AP_FDUPFD_SYMBOL,AP_EPOLL_CTL_SYMBOL};
            static const unsigned long long outer_cookies[]={AP_ACCEPT_ENTRY_COOKIE,
                AP_CONNECT_ENTRY_COOKIE,AP_FDUPFD_COOKIE,AP_EPOLL_CTL_COOKIE};
            static const char *inner_symbols[]={AP_CONNECT_SECURITY_SYMBOL,AP_CONNECT_AUDIT_SYMBOL,
                AP_ALLOC_FD_SYMBOL,AP_FILE_SYMBOL,AP_READ_SELECTED_SYMBOL};
            static const unsigned long long inner_cookies[]={AP_CONNECT_SECURITY_COOKIE,
                AP_CONNECT_AUDIT_COOKIE,AP_FDUPFD_ALLOC_COOKIE,AP_FILE_FDGET_COOKIE,
                AP_READ_FDGET_COOKIE};
            static const char *shared_symbols[]={AP_FDGET_SYMBOL,AP_STREAM_TX_SYMBOL,
                AP_STREAM_TX_LOCK_SYMBOL,AP_STREAM_TX_UNLOCK_SYMBOL};
            static const unsigned long long shared_cookies[]={AP_SHARED_FDGET_COOKIE,AP_STREAM_TX_COOKIE,
                AP_STREAM_TX_LOCK_COOKIE,AP_STREAM_TX_UNLOCK_COOKIE};
            const unsigned long long retirement_cookie=retirement==0?AP_FILE_RETIRE_COOKIE:AP_EXEC_CLOSE_COOKIE;
#endif
            struct ap_kprobe_multi_opts options={.sz=sizeof(options),.syms=symbols,
                .cookies=fdget?cookies:
#ifdef AP_FTRACE_PROVIDER
                    &retirement_cookie,
#else
                    NULL,
#endif
                .cnt=fdget?AP_ACTIVE_CONNECT_SESSION_SITES:1,.session=fdget};
#ifdef AP_FTRACE_PROVIDER
            if(fdget) {
                options.syms=fdget_group?inner_symbols:outer_symbols;
                options.cookies=fdget_group?inner_cookies:outer_cookies;
                options.cnt=fdget_group?5:4;
            } else if(fdget_shared>=0) {
                options.syms=shared_symbols;options.cookies=shared_cookies;options.cnt=4;
                options.retprobe=fdget_shared==1;options.session=false;
            }
#endif
            link=bpf_program__attach_kprobe_multi_opts(p,NULL,&options);
        } else if(selection>=0) {
#ifdef AP_FTRACE_PROVIDER
            return unavailable();
#elif defined(AP_GROUPED_PROVIDER)
            link=grouped_attach(s,p);
#else
            struct ap_kprobe_opts options={.sz=sizeof(options),.bpf_cookie=AP_CONNECT_POST_COOKIE,
                .offset=AP_CONNECT_FDGET_RETURN_OFFSET,.retprobe=false,.attach_mode=3};
            link=bpf_program__attach_kprobe_opts(p,AP_CONNECT_SYMBOL,&options);
#endif
        } else {
#ifdef AP_FTRACE_PROVIDER
            int generic_fd=bpf_program__fd(p);struct bpf_prog_info generic={0};
            u32 generic_size=sizeof(generic);
            if(generic_fd<0 || bpf_obj_get_info_by_fd(generic_fd,&generic,&generic_size) ||
               generic_size<offsetof(struct bpf_prog_info,type)+sizeof(generic.type) ||
               generic.type!=BPF_PROG_TYPE_TRACING)return unavailable();
#endif
            link=bpf_program__attach(p);
        }
        error=libbpf_get_error(link);
        if(!link || error) { if(error)errno=-error; return -1; }
        s->links[s->links_count++]=link;
        if(selection>=0) {
            if(s->selection_program[selection]!=-1 || s->selection_link[selection]!=-1)return unavailable();
            s->selection_program[selection]=bpf_program__fd(p);s->selection_link[selection]=bpf_link__fd(link);
        }
        if(fdget) {
#ifdef AP_FTRACE_PROVIDER
            int *program=fdget_group?&s->fdget_extra_program:&s->fdget_program;
            int *link_fd=fdget_group?&s->fdget_extra_link:&s->fdget_link;
            if(*program!=-1 || *link_fd!=-1)return unavailable();
            *program=bpf_program__fd(p);*link_fd=bpf_link__fd(link);
#else
            if(s->fdget_program!=-1 || s->fdget_link!=-1)return unavailable();
            s->fdget_program=bpf_program__fd(p);s->fdget_link=bpf_link__fd(link);
#endif
        }
#ifdef AP_FTRACE_PROVIDER
        if(fdget_shared>=0) {
            if(s->fdget_shared_program[fdget_shared]!=-1 || s->fdget_shared_link[fdget_shared]!=-1)
                return unavailable();
            s->fdget_shared_program[fdget_shared]=bpf_program__fd(p);
            s->fdget_shared_link[fdget_shared]=bpf_link__fd(link);
        }
#endif
        if(read_entry) {
            if(s->read_entry_program!=-1 || s->read_entry_link!=-1)return unavailable();
            s->read_entry_program=bpf_program__fd(p);s->read_entry_link=bpf_link__fd(link);
        }
#ifdef AP_FTRACE_PROVIDER
        if(retirement>=0) {
            if(s->retirement_program[retirement]!=-1 || s->retirement_link[retirement]!=AP_LINKS)
                return unavailable();
            s->retirement_program[retirement]=bpf_program__fd(p);
            s->retirement_link[retirement]=s->links_count-1;
        }
        if(fault>=0) {
            if(s->fault_program[fault]!=-1 || s->fault_link[fault]!=AP_LINKS)return unavailable();
            s->fault_program[fault]=bpf_program__fd(p);
            s->fault_link[fault]=s->links_count-1;
        }
#endif
        if(stream>=0) {
            if(s->stream_program[stream]!=-1)return unavailable();
            s->stream_program[stream]=bpf_program__fd(p);s->stream_link[stream]=s->links_count-1;
        }
#ifndef AP_FTRACE_PROVIDER
        if(selection==1)selection_shared=p;
#endif
#ifndef AP_GROUPED_PROVIDER
        if(stream==2 || stream==3)stream_shared[stream-2]=p;
#endif
        if(bind_observer_link(s,s->links_count-1,p,false))return -1;
        /* The actual link owns the program now. Release only redundant load
         * handles; admission-counter and exact-target readers keep their FDs.
         * Do this per pair so the additional perf/link never raises FD128. */
        if(!fdget && fdget_shared<0 && !read_entry && selection<0 && stream<0
#ifdef AP_FTRACE_PROVIDER
           && retirement<0 && fault<0
#endif
          )bpf_program__unload(p);
    }
#ifdef AP_FTRACE_PROVIDER
    if(s->links_count!=AP_PROGRAMS || s->read_entry_program<0 || s->read_entry_link<0 ||
       grouped_anchor(s,config))return -1;
#elif defined(AP_GROUPED_PROVIDER)
    if(s->links_count!=AP_PROGRAMS || !selection_shared)return unavailable();
    for(unsigned i=0;i<9;i++) {s->selection_program[i]=bpf_program__fd(selection_shared);s->selection_link[i]=s->selection_link[1];}
    for(unsigned i=0;i<2;i++) {s->copy_program[i]=s->selection_program[1];s->copy_link[i]=s->selection_link[1];}
    u32 at=AP_LINKS;
    for(u32 i=0;i<s->links_count;i++)if(bpf_link__fd(s->links[i])==s->selection_link[1]) {
        if(at!=AP_LINKS)return unavailable();at=i;
    }
    if(at==AP_LINKS || grouped_anchor(s,config))return -1;
    for(unsigned i=2;i<6;i++) {s->stream_program[i]=s->selection_program[1];s->stream_link[i]=at;}
    if(ap_grouped_io_activate(io,owner,deadline))return -1;
#else
    if(s->links_count!=AP_PROGRAMS || !selection_shared || !stream_shared[0] || !stream_shared[1])return unavailable();
    /* Every former standalone classic attachment remains owned. The single
     * loaded dispatcher handles copy-before/after, Accept, File, Read and Ctl
     * by exact cookies; each link still has an independent kernel identity. */
    static const struct {const char *symbol;u64 offset,cookie;int copy,selection;} sites[]={
        {AP_CONNECT_SYMBOL,AP_CONNECT_COPY_BEFORE_OFFSET,AP_CONNECT_COPY_BEFORE_COOKIE,0,-1},
        {AP_CONNECT_SYMBOL,AP_CONNECT_COPY_AFTER_OFFSET,AP_CONNECT_COPY_AFTER_COOKIE,1,-1},
        {AP_ACCEPT_SYMBOL,AP_ACCEPT_FDGET_RETURN_OFFSET,AP_ACCEPT_POST_COOKIE,-1,0},
        {AP_FILE_SYMBOL,AP_FILE_FDGET_RETURN_OFFSET,AP_FILE_POST_COOKIE,-1,2},
        {AP_FILE_SYMBOL,AP_FILE_ENTRY_OFFSET,AP_FILE_ENTRY_COOKIE,-1,3},
        {AP_READ_ENTRY_SYMBOL,AP_READ_ENTRY_OFFSET,AP_READ_ENTRY_COOKIE,-1,4},
        {AP_READ_SELECTED_SYMBOL,AP_READ_SELECTED_OFFSET,AP_READ_SELECTED_COOKIE,-1,5},
        {AP_READ_SELECTED_SYMBOL,AP_READ_NULL_OFFSET,AP_READ_NULL_COOKIE,-1,6},
        {AP_EPOLL_CTL_SYMBOL,AP_EPOLL_PRIMARY_POST_OFFSET,AP_EPOLL_PRIMARY_POST_COOKIE,-1,7},
        {AP_EPOLL_CTL_SYMBOL,AP_EPOLL_TARGET_POST_OFFSET,AP_EPOLL_TARGET_POST_COOKIE,-1,8},
    };
    _Static_assert(sizeof(sites)/sizeof(sites[0])==AP_SHARED_SELECTION_LINKS,"complete shared classic inventory");
    for(unsigned which=0;which<AP_SHARED_SELECTION_LINKS;which++) {
        struct ap_kprobe_opts options={.sz=sizeof(options),.bpf_cookie=sites[which].cookie,
            .offset=sites[which].offset,.retprobe=false,.attach_mode=3};
        struct bpf_link *link=bpf_program__attach_kprobe_opts(selection_shared,sites[which].symbol,&options);
        error=libbpf_get_error(link);
        if(!link || error) {if(error)errno=-error;return -1;}
        s->links[s->links_count++]=link;
        if(sites[which].copy>=0) {
            s->copy_program[sites[which].copy]=bpf_program__fd(selection_shared);
            s->copy_link[sites[which].copy]=bpf_link__fd(link);
        } else {
            s->selection_program[sites[which].selection]=bpf_program__fd(selection_shared);
            s->selection_link[sites[which].selection]=bpf_link__fd(link);
        }
        if(bind_observer_link(s,AP_PROGRAMS+which,selection_shared,true))return -1;
    }
    /* Fragment copies use a second actual call/successor pair in the same
     * function. Each owns a separate exact link; its program is shared. */
    for(unsigned which=0;which<2;which++) {
        struct ap_kprobe_opts options={.sz=sizeof(options),
            .bpf_cookie=which?AP_STREAM_COPY_FRAG_EXIT_COOKIE:AP_STREAM_COPY_FRAG_ENTRY_COOKIE,
            .offset=which?AP_STREAM_COPY_FRAG_EXIT_OFFSET:AP_STREAM_COPY_FRAG_ENTRY_OFFSET,
            .retprobe=false,.attach_mode=3};
        struct bpf_link *link=bpf_program__attach_kprobe_opts(stream_shared[which],AP_STREAM_COPY_SYMBOL,&options);
        error=libbpf_get_error(link);
        if(!link || error) {if(error)errno=-error;return -1;}
        s->links[s->links_count++]=link;
        s->stream_program[4+which]=bpf_program__fd(stream_shared[which]);
        s->stream_link[4+which]=s->links_count-1;
        if(bind_observer_link(s,AP_PROGRAMS+AP_SHARED_SELECTION_LINKS+which,stream_shared[which],true))return -1;
    }
#endif
    return finish_observer_load(s);
}
int ap_register_task(struct ap_session *s,int pidfd) {
    if(pidfd<0)return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.provider=s->incarnation};
    int rc=bpf_map_update_elem(s->tasks,&pidfd,&c,BPF_NOEXIST);
    leave_commands(s);return rc;
}
/* The caller retains the exact auxiliary worker PIDFD from its one successful
 * registration through collection and the original command ACK. Only that
 * owner may retire this idle registration; a numeric TID is not authority.
 * No retry/ENOENT exemption: unknown deletion stays a failed owned effect. */
int ap_retire_auxiliary_task(struct ap_session *s,int pidfd) {
    if(pidfd<0)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;struct ap_task_command observed,idle={.provider=s->incarnation};
    if(bpf_map_lookup_elem(s->tasks,&pidfd,&observed))goto done;
    if(memcmp(&observed,&idle,sizeof(idle))) {errno=EBUSY;goto done;}
    if(bpf_map_delete_elem(s->tasks,&pidfd))goto done;
    if(!bpf_map_lookup_elem(s->tasks,&pidfd,&observed)) {errno=EPROTO;goto done;}
    if(errno!=ENOENT)goto done;
    rc=0;
done:
    leave_commands(s);return rc;
}
/* The session owns every reserved ticket even when a map syscall returns an
 * unknown outcome. No cancellation or ordinary error frees that reservation. */
static int submit_with_executable(struct ap_session *s,int pidfd,struct ap_task_command *c,
        const struct ap_executable_intent *intent) {
    if((c->operation==AP_EXECUTABLE_SOURCE)!=(intent!=NULL))return invalid();
    if(pidfd<0 || !ap_task_command_extension_valid(c))return invalid();
    struct ap_task_command prior;
    if(bpf_map_lookup_elem(s->tasks,&pidfd,&prior))return -1;
    struct ap_task_command idle={.provider=s->incarnation};
    if(memcmp(&prior,&idle,sizeof(prior))) { errno=EBUSY; return -1; }
    u32 key=0;
    for(u32 scanned=0;scanned<AP_COMMAND_SLOTS;scanned++) {
        if(s->next_command==UINT64_MAX) { errno=EOVERFLOW; return -1; }
        u64 ticket=++s->next_command;
        u32 candidate=ap_command_slot(ticket);
        if(s->pending[candidate].state==AP_SLOT_FREE) { key=candidate;c->command=ticket;break; }
    }
    if(!key) { errno=ENOSPC; return -1; }
    c->provider=s->incarnation;
    struct ap_pending_command *p=&s->pending[key];
    p->state=AP_SLOT_RESERVED;p->submitted=*c;
    struct ap_command_result prior_result,empty={0};
    if(bpf_map_lookup_elem(s->commands,&key,&prior_result))return quarantine(p);
    if(memcmp(&prior_result,&empty,sizeof(empty))) { errno=EPROTO;return quarantine(p); }
    struct ap_command_result reserved={.command=c->command,.operation=c->operation,
        .phase=AP_COMMAND_READY,.original_count=c->original_count};
    if(bpf_map_update_elem(s->commands,&key,&reserved,BPF_EXIST))return quarantine(p);
    if(fd_reserve_accept(s,c) || fd_reserve_enrollment(s,c) || executable_reserve(s,c,intent))return quarantine(p);
    if(bpf_map_update_elem(s->tasks,&pidfd,c,BPF_EXIST))return quarantine(p);
    p->state=AP_SLOT_ACTIVE;return 0;
}
static int submit(struct ap_session *s,int pidfd,struct ap_task_command *c) {
    return submit_with_executable(s,pidfd,c,NULL);
}
/* DONE is immutable and is the last producer access to this result. A second
 * complete lookup after observing DONE rejects a payload copied across that
 * publication. No reader races an ACK: command_busy covers this entire path. */
static int read_command_completion(struct ap_session *s,u64 seq,u64 operation,struct ap_command_result *r) {
    if(!r || !seq)return invalid();
    u32 key=ap_command_slot(seq);struct ap_pending_command *p=&s->pending[key];
    if(p->state!=AP_SLOT_ACTIVE || p->submitted.command!=seq || p->submitted.operation!=operation)return invalid();
    struct ap_command_result first;
    if(bpf_map_lookup_elem(s->commands,&key,&first))return -1;
    *r=first; /* Raw failure evidence, not a completed-custody assertion. */
    if(first.phase!=AP_COMMAND_DONE || first.command!=seq || first.operation!=operation || !first.task || !first.start_boottime)return unavailable();
    atomic_thread_fence(memory_order_acquire);
    if(bpf_map_lookup_elem(s->commands,&key,r))return -1;
    if(memcmp(&first,r,sizeof(first)))return unavailable();
    struct ap_status state;
    if(ap_read_status(s,&state))return -1;
    if(state.fatal)return unavailable();
    return 0;
}
static int read_completion(struct ap_session *s,int pidfd,u64 seq,u64 operation,struct ap_command_result *r) {
    if(!r || !seq || pidfd<0)return invalid();
    struct ap_pending_command *p=&s->pending[ap_command_slot(seq)];
    if(p->state!=AP_SLOT_ACTIVE || p->submitted.command!=seq || p->submitted.operation!=operation)return invalid();
    struct ap_task_command active;
    if(bpf_map_lookup_elem(s->tasks,&pidfd,&active))return -1;
    if(memcmp(&active,&p->submitted,sizeof(active)))return invalid();
    return read_command_completion(s,seq,operation,r);
}
/* Retain the receipt before clearing the exact task command. Clearing makes
 * this task available for a different free slot, never this unacknowledged one. */
static int disarm_task_observed(struct ap_session *s,int pidfd,struct ap_pending_command *p) {
    p->state=AP_SLOT_DISARMING;
    p->disarm=(struct ap_task_disarm_outcome){0};
    struct ap_task_command cleared={.provider=s->incarnation},observed;
    int rc=bpf_map_update_elem(s->tasks,&pidfd,&cleared,BPF_EXIST),error=errno;
    p->disarm.phase=AP_DISARM_IDLE_UPDATE_RETURNED;
    p->disarm.mutation_rc=rc;p->disarm.mutation_errno=rc?error:0;
    if(rc) {errno=error;return -1;}
    rc=bpf_map_lookup_elem(s->tasks,&pidfd,&observed);error=errno;
    p->disarm.phase=AP_DISARM_IDLE_READBACK_RETURNED;
    p->disarm.readback_rc=rc;p->disarm.readback_errno=rc?error:0;
    if(rc) {errno=error;return -1;}
    if(memcmp(&cleared,&observed,sizeof(cleared))) { errno=EPROTO;return -1; }
    return 0;
}
static int disarm_task(struct ap_session *s,int pidfd,struct ap_pending_command *p) {
    return disarm_task_observed(s,pidfd,p)?quarantine(p):0;
}
static int collect(struct ap_session *s,int pidfd,const struct ap_command_result *r) {
    struct ap_pending_command *p=&s->pending[ap_command_slot(r->command)];
    p->receipt=*r;
    if(disarm_task(s,pidfd,p))return -1;
    p->state=AP_SLOT_COLLECTED;return 0;
}
static int probe(struct ap_session *s,int pidfd,int held,u64 operation,u64 generation,
                 struct ap_identity expected,struct ap_command_result *r) {
    if(held<0 || !r)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;
    struct ap_task_command c={.operation=operation,.expected_object=expected.object,.generation_before=generation};
    struct ap_command_result observed={0};
    if(submit(s,pidfd,&c))goto done;
    u64 cookie=0;socklen_t size=sizeof(cookie);
    int primary=getsockopt(held,SOL_SOCKET,SO_COOKIE,&cookie,&size);
    int primary_errno=errno;
    int completion=read_completion(s,pidfd,c.command,operation,&observed);
    *r=observed; /* Preserve v10 raw diagnostics even when later validation fails. */
    if(completion)goto done;
    if(primary || observed.returned || size!=sizeof(cookie) || !cookie || cookie!=observed.cookie ||
       observed.identity.provider!=s->incarnation ||
       (operation!=AP_OBSERVE_SOCKET_FILE && !observed.identity.object) || !observed.identity.namespace) {
        errno=primary?primary_errno:EPROTO;goto done;
    }
    if(operation==AP_MATCH && ((expected.object && memcmp(&expected,&observed.identity,sizeof(expected))) || !observed.creation)) { invalid();goto done; }
    if(collect(s,pidfd,&observed))goto done;
    *r=observed;rc=0;
done:
    leave_commands(s);return rc;
}
int ap_observe_socket_file(struct ap_session *s,int pidfd,int held,struct ap_command_result *r) {
    if(!s)return invalid();
    struct ap_identity none={0};return probe(s,pidfd,held,AP_OBSERVE_SOCKET_FILE,0,none,r);
}
int ap_enroll_listener(struct ap_session *s,int pidfd,int held,u64 generation,struct ap_command_result *r) {
    struct ap_identity none={0};return probe(s,pidfd,held,AP_ENROLL,generation,none,r);
}
int ap_prepare_setter(struct ap_session *s,int pidfd,struct ap_identity id,u64 before,u64 after,
                      int level,int option,u64 *seq) {
    if(!s || !seq || id.provider!=s->incarnation || !id.object || !id.namespace || before==UINT64_MAX || after!=before+1)return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.operation=AP_SETTER,.expected_object=id.object,
        .generation_before=before,.generation_after=after,.expected_level=level,.expected_option=option};
    int rc=submit(s,pidfd,&c);
    if(!rc)*seq=c.command;
    leave_commands(s);return rc;
}
int ap_finish_setter(struct ap_session *s,int pidfd,u64 seq,struct ap_command_result *r) {
    if(!r)return invalid();
    if(enter_commands(s))return -1;
    struct ap_command_result observed={0};
    int rc=read_completion(s,pidfd,seq,AP_SETTER,&observed);
    *r=observed; /* A failed observation remains raw and explicitly failed. */
    if(!rc)rc=collect(s,pidfd,&observed);
    if(!rc && r)*r=observed;
    leave_commands(s);return rc;
}
int ap_resolve_accepted(struct ap_session *s,int pidfd,int held,struct ap_command_result *r) {
    struct ap_identity any={0};return probe(s,pidfd,held,AP_MATCH,0,any,r);
}
int ap_match_accepted(struct ap_session *s,int pidfd,int held,struct ap_identity expected,struct ap_command_result *r) {
    if(!s || expected.provider!=s->incarnation || !expected.object || !expected.namespace)return invalid();
    return probe(s,pidfd,held,AP_MATCH,0,expected,r);
}
int ap_ack_command(struct ap_session *s,const struct ap_command_result *receipt) {
    if(!receipt || !receipt->command)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;u32 key=ap_command_slot(receipt->command);
    struct ap_pending_command *p=&s->pending[key];
    if(p->state!=AP_SLOT_COLLECTED || p->submitted.command!=receipt->command ||
       receipt->identity.provider!=s->incarnation || memcmp(&p->receipt,receipt,sizeof(*receipt))) { errno=ESTALE;goto done; }
    struct ap_command_result observed;
    if(bpf_map_lookup_elem(s->commands,&key,&observed))goto done;
    if(memcmp(&observed,receipt,sizeof(observed))) { errno=EPROTO;quarantine(p);goto done; }
    struct ap_status status;
    if(ap_read_status(s,&status))goto done;
    if(status.fatal) { unavailable();goto done; }
    /* Producer is done, exact task command is disarmed, all driver readers are
     * excluded, and the service acknowledged its retained receipt. */
    struct ap_command_result empty={0};
    if(fd_ack_accept(s,p) || fd_ack_enrollment(s,p) || fd_ack_original(s,p) || fd_ack_birth(s,p) || fd_ack_epoll_ctl_copy(s,p) || executable_ack(s,p)) { quarantine(p);goto done; }
    if(bpf_map_update_elem(s->commands,&key,&empty,BPF_EXIST)) { quarantine(p);goto done; }
    if(bpf_map_lookup_elem(s->commands,&key,&observed)) { quarantine(p);goto done; }
    if(memcmp(&observed,&empty,sizeof(empty))) { errno=EPROTO;quarantine(p);goto done; }
    memset(p,0,sizeof(*p));rc=0;
done:
    leave_commands(s);return rc;
}
int ap_read_creation(struct ap_session *s,u32 seq,struct ap_creation *r) {
    if(!s || !s->ready || !r || !seq || seq>=AP_EVENTS)return invalid();
    if(bpf_map_lookup_elem(s->events,&seq,r))return -1;
    if(!(r->phase&AP_CREATED) || r->sequence!=seq)return unavailable();
    return 0;
}
int ap_read_status(struct ap_session *s,struct ap_status *r) {
    if(!s || !s->ready || !r)return invalid();
    u32 zero=0;return bpf_map_lookup_elem(s->status,&zero,r);
}
int ap_read_setter_rejection(struct ap_session *s,struct ap_setter_rejection *r) {
    if(!s || !s->ready || !r)return invalid();
    u32 key=0;
    if(bpf_map_lookup_elem(s->commands,&key,r))return -1;
    return r->phase==2?0:unavailable();
}
static bool same_inherited(const struct ap_raw_state *a,const struct ap_raw_state *b,bool child) {
    return a->receive_timeout_ticks==b->receive_timeout_ticks && a->send_timeout_ticks==b->send_timeout_ticks &&
        a->lowat==b->lowat && a->receive_buffer==b->receive_buffer && a->peek_offset==b->peek_offset &&
        (child ? (u8)(a->userlocks & ~8U) : a->userlocks)==b->userlocks && a->scaling_ratio==b->scaling_ratio;
}
int ap_validate_creation(const struct ap_creation *e,const struct ap_status *s) {
    if(!e || !s || s->fatal || !e->sequence || e->sequence>=AP_EVENTS ||
       !(e->phase&AP_CREATED) || !(e->phase&AP_QUEUED) || e->phase&AP_RETIRED ||
       e->overlap || e->mutation_epoch_enter!=e->mutation_epoch_exit ||
       !e->listener.provider || e->listener.provider!=e->child.provider ||
       !e->listener.namespace || e->listener.namespace!=e->child.namespace ||
       !e->listener.object || !e->child.object || !e->cookie_at_creation || e->listener.object==e->child.object ||
       e->listener_before.tcp_state!=AP_TCP_LISTEN || e->listener_after.tcp_state!=AP_TCP_LISTEN ||
       !e->child_created.child_spin_locked || e->child_created.tcp_state!=AP_TCP_SYN_RECV ||
       e->local.family!=AP_AF_INET || e->peer.family!=AP_AF_INET ||
       !e->local.address_be || !e->peer.address_be || !e->local.port_be || !e->peer.port_be ||
       !same_inherited(&e->listener_before,&e->listener_after,false) ||
       !same_inherited(&e->listener_before,&e->child_created,true))return unavailable();
    return 0;
}
static int append_identifier(struct ap_program_id *out,u32 capacity,u32 *n,u32 kind,u32 id) {
    if(!id) {errno=EOVERFLOW;return -1;}
    /* A retained program FD and its link identify the same program. IDs in
     * different kernel object namespaces are never collapsed together. */
    for(u32 i=0;i<*n;i++)if(out[i].kind==kind && out[i].id==id)return 0;
    if(*n>=capacity) {errno=EOVERFLOW;return -1;}
    out[(*n)++]=(struct ap_program_id){kind,id};return 0;
}
static int add_identifier(struct ap_program_id *out,u32 capacity,u32 *n,u32 kind,int fd,u32 expected) {
    if(fd<0)return 0; /* not yet loaded: no invented ID */
    union { struct bpf_map_info map;struct bpf_prog_info prog; } info={0};
    u32 size=kind==0?sizeof(info.map):sizeof(info.prog);
    if(bpf_obj_get_info_by_fd(fd,&info,&size))return -1;
    u32 minimum=kind==0?offsetof(struct bpf_map_info,id)+sizeof(info.map.id):
        offsetof(struct bpf_prog_info,id)+sizeof(info.prog.id);
    if(size<minimum)return unavailable();
    u32 id=kind==0?info.map.id:info.prog.id;
    if(expected && id!=expected)return unavailable();
    return append_identifier(out,capacity,n,kind,id);
}
int ap_identifiers(struct ap_session *s,struct ap_program_id *out,u32 capacity,u32 *written) {
    if(!s || !out || !written)return invalid();
    *written=0;
    if(s->links_count>AP_LINKS)return unavailable();
    if(s->object) {
        struct bpf_map *m=NULL;while((m=bpf_object__next_map(s->object,m)))
            if(add_identifier(out,capacity,written,0,bpf_map__fd(m),0))return -1;
        struct bpf_program *p=NULL;u32 at=0;
        while((p=bpf_object__next_program(s->object,p))) {
            int fd=bpf_program__fd(p);
            /* A linked program may lose its userspace FD only after its own
             * positive binding. Unlinked programs keep original FDs even on
             * partial attachment; linked programs retain their exact IDs. */
            if(fd<0 && at<s->links_count && !s->link_identity[at].program_id)return unavailable();
            if(add_identifier(out,capacity,written,1,fd,at<s->links_count?s->link_identity[at].program_id:0))return -1;
            at++;
        }
    }
    for(u32 i=0;i<s->links_count;i++) {
        struct bpf_link_info link;
        if(owned_link_info(s,i,&link) ||
           append_identifier(out,capacity,written,1,link.prog_id) ||
           append_identifier(out,capacity,written,2,link.id))return -1;
    }
    return 0;
}
int ap_close(struct ap_session *s) {
    if(!s)return 0;
    int failed=0;
#ifdef AP_FTRACE_PROVIDER
    /* Terminal only: active metadata readers retain their load FDs until here.
     * Drop those references before detaching links, while all original links,
     * maps and the ring remain owned. Failed load/attach may leave programs
     * without FDs or without links; unload only still-owned load handles.
     * This ordering is not a kernel object-absence or deferred-free barrier. */
    if(s->object) {
        struct bpf_program *p=NULL;
        while((p=bpf_object__next_program(s->object,p)))
            if(bpf_program__fd(p)>=0)bpf_program__unload(p);
    }
#endif
    for(u32 i=s->links_count;i>0;i--)if(bpf_link__destroy(s->links[i-1]))failed=1;
    if(s->stream_copy_ring)ring_buffer__free(s->stream_copy_ring);
    for(u32 i=0;i<AP_COMMANDS;i++)free(s->pending[i].stream_copy.records);
    if(s->object)bpf_object__close(s->object);
    free(s);
    return failed?-1:0;
}

#if defined(AP_GROUPED_PROVIDER) && !defined(AP_FTRACE_PROVIDER)
int ap_close_grouped_terminal(struct ap_session **owned,u64 release_start) {
    if(!owned || !*owned)return invalid();
    u64 query_deadline;if(grouped_deadline(&query_deadline))return -1;
    u64 now=query_deadline-2000000000ULL;
    if(!release_start || release_start>now || release_start>~0ULL-1000000000ULL)return invalid();
    if(now>=release_start+1000000000ULL) {errno=ETIMEDOUT;return -1;}
    struct ap_session *s=*owned;
    if(!s->group_io || !s->group_owner || s->group_owner->phase!=AP_GROUPED_ACTIVE)return invalid();
    for(unsigned i=0;i<AP_COMMANDS;i++)if(s->pending[i].state!=AP_SLOT_FREE)return unavailable();
    struct ap_grouped_io *io=s->group_io;struct ap_grouped_owner *owner=s->group_owner;
    if(ap_grouped_quiescent(owner))return -1;
    s->ready=false;*owned=NULL;
    int closed=ap_close(s),saved=errno;
    int deleted=ap_grouped_io_delete(io,owner,release_start);
    if(closed) {errno=saved;return -1;}
    return deleted;
}
int ap_close_grouped_terminal_until(struct ap_session **owned,u64 release_start,u64 enclosing_cutoff) {
    if(!owned || !*owned)return invalid();
    u64 query_deadline;if(grouped_deadline(&query_deadline))return -1;
    u64 now=query_deadline-2000000000ULL;
    if(!release_start || release_start>now || release_start>~0ULL-1000000000ULL)return invalid();
    if(now>=release_start+1000000000ULL||now>=enclosing_cutoff) {errno=ETIMEDOUT;return -1;}
    struct ap_session *s=*owned;
    if(!s->group_io || !s->group_owner || s->group_owner->phase!=AP_GROUPED_ACTIVE)return invalid();
    for(unsigned i=0;i<AP_COMMANDS;i++)if(s->pending[i].state!=AP_SLOT_FREE)return unavailable();
    struct ap_grouped_io *io=s->group_io;struct ap_grouped_owner *owner=s->group_owner;
    if(ap_grouped_quiescent(owner))return -1;
    s->ready=false;*owned=NULL;
    int closed=ap_close(s),saved=errno;
    int deleted=ap_grouped_io_delete_until(io,owner,release_start,enclosing_cutoff);
    if(closed) {errno=saved;return -1;}
    return deleted;
}
int ap_close_grouped_startup_terminal(struct ap_session **owned,struct ap_grouped_io *io,
        struct ap_grouped_owner *owner,u64 release_start,u64 enclosing_cutoff) {
    if(!owned||!io||!owner)return invalid();
    u64 query_deadline;if(grouped_deadline(&query_deadline))return -1;
    u64 now=query_deadline-2000000000ULL;
    if(!release_start||release_start>now||release_start>~0ULL-1000000000ULL)return invalid();
    if(now>=release_start+1000000000ULL||now>=enclosing_cutoff) {errno=ETIMEDOUT;return -1;}
    /* Failed final validation can leave ACTIVE; preserve the exact existing
     * active close, including all pending-command and quiescent checks. */
    if(owner->phase==AP_GROUPED_ACTIVE) {
        if(!*owned||(*owned)->ready||(*owned)->group_io!=io||(*owned)->group_owner!=owner)return invalid();
        return ap_close_grouped_terminal_until(owned,release_start,enclosing_cutoff);
    }
    if((owner->phase!=AP_GROUPED_LEAVES&&owner->phase!=AP_GROUPED_UNKNOWN)||
       owner->attempted_sites!=AP_GROUPED_ALL_SITES)return invalid();
    struct ap_session *s=*owned;
    if(s) {
        if(s->ready||s->incarnation!=owner->incarnation||s->group_io!=io||s->group_owner!=owner)return invalid();
        for(unsigned i=0;i<AP_COMMANDS;i++)if(s->pending[i].state!=AP_SLOT_FREE)return unavailable();
    }
    int closed=0,saved=0;
    if(s) {*owned=NULL;closed=ap_close(s);saved=errno;}
    /* A failed close remains the primary result even if bounded cleanup can
     * subsequently reconcile/delete. No C owner or failure is reset. */
    int recovered=ap_grouped_io_recover_terminal(io,owner,release_start,enclosing_cutoff);
    int deleted=recovered?-1:ap_grouped_io_delete_until(io,owner,release_start,enclosing_cutoff);
    if(closed) {errno=saved;return -1;}
    return deleted;
}
#endif

#include "stream-tx-driver.h"
#include "stream-copy-driver.h"
#include "fd-effects-driver.h"
#include "epoll-ctl-copy-driver.h"

#include "fd-enrollment-driver.h"
#include "executable-source-driver.h"

#include "owned-metadata-driver.h"
