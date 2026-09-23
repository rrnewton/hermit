// Read-only BPF provider. All socket descriptors are borrowed, never closed here.
#define _GNU_SOURCE
#include <errno.h>
#include <linux/bpf.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
#include "provider.h"
#include "fd-effects.h"
struct bpf_object;
struct bpf_program;
struct bpf_link;
struct bpf_map;
extern struct bpf_object *bpf_object__open_file(const char *,const void *);
extern long libbpf_get_error(const void *);
extern int bpf_object__load(struct bpf_object *);
extern void bpf_object__close(struct bpf_object *);
extern struct bpf_program *bpf_object__next_program(const struct bpf_object *,struct bpf_program *);
extern struct bpf_map *bpf_object__next_map(const struct bpf_object *,const struct bpf_map *);
extern struct bpf_link *bpf_program__attach(const struct bpf_program *);
extern int bpf_program__fd(const struct bpf_program *);
extern int bpf_link__fd(const struct bpf_link *);
extern int bpf_link__destroy(struct bpf_link *);
extern int bpf_map__fd(const struct bpf_map *);
extern int bpf_object__find_map_fd_by_name(const struct bpf_object *,const char *);
extern int bpf_map_update_elem(int,const void *,const void *,unsigned long long);
extern int bpf_map_lookup_elem(int,const void *,void *);
extern int bpf_map_delete_elem(int,const void *);
extern int bpf_obj_get_info_by_fd(int,void *,unsigned int *);
#define AP_PROGRAMS 25
enum ap_slot_state { AP_SLOT_FREE, AP_SLOT_RESERVED, AP_SLOT_ACTIVE, AP_SLOT_DISARMING, AP_SLOT_COLLECTED, AP_SLOT_QUARANTINED };
struct ap_pending_command {
    enum ap_slot_state state;
    struct ap_task_command submitted;
    struct ap_command_result receipt;
    struct ap_fd_accept fd_receipt;
    bool fd_collected;
};
static int fd_reserve_accept(struct ap_session *,const struct ap_task_command *);
static int fd_ack_accept(struct ap_session *,struct ap_pending_command *);
struct ap_session {
    struct bpf_object *object;
    struct bpf_link *links[AP_PROGRAMS];
    u32 links_count;
    int tasks,commands,events,status;
    u64 incarnation,next_command;
    bool ready;
    atomic_flag command_busy; /* Nonblocking serialization of submit/read/ACK. */
    struct ap_pending_command pending[AP_COMMANDS];
};
static int unavailable(void) { errno=ENODATA; return -1; }
static int invalid(void) { errno=EINVAL; return -1; }
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
int ap_open(const char *path,u64 incarnation,struct ap_session **out) {
    if(!path || !incarnation || !out)return invalid();
    *out=NULL;
    struct ap_session *s=calloc(1,sizeof(*s));
    if(!s)return -1;
    *out=s; /* even partial load/attach failure returns owned cleanup inventory */
    s->tasks=s->commands=s->events=s->status=-1;
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
    u32 zero=0;struct ap_config c={.provider=incarnation};
    if(bpf_map_update_elem(config,&zero,&c,BPF_ANY))return -1;
    struct bpf_program *p=NULL;
    while((p=bpf_object__next_program(s->object,p))) {
        if(s->links_count==AP_PROGRAMS) { errno=EOVERFLOW; return -1; }
        struct bpf_link *link=bpf_program__attach(p);
        error=libbpf_get_error(link);
        if(!link || error) { if(error)errno=-error; return -1; }
        s->links[s->links_count++]=link;
    }
    if(s->links_count!=AP_PROGRAMS)return unavailable();
    s->ready=true;return 0;
}
int ap_register_task(struct ap_session *s,int pidfd) {
    if(pidfd<0)return invalid();
    if(enter_commands(s))return -1;
    struct ap_task_command c={.provider=s->incarnation};
    int rc=bpf_map_update_elem(s->tasks,&pidfd,&c,BPF_NOEXIST);
    leave_commands(s);return rc;
}
/* The session owns every reserved ticket even when a map syscall returns an
 * unknown outcome. No cancellation or ordinary error frees that reservation. */
static int submit(struct ap_session *s,int pidfd,struct ap_task_command *c) {
    if(pidfd<0)return invalid();
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
    struct ap_command_result reserved={.command=c->command,.operation=c->operation,.phase=AP_COMMAND_READY};
    if(bpf_map_update_elem(s->commands,&key,&reserved,BPF_EXIST))return quarantine(p);
    if(fd_reserve_accept(s,c))return quarantine(p);
    if(bpf_map_update_elem(s->tasks,&pidfd,c,BPF_EXIST))return quarantine(p);
    p->state=AP_SLOT_ACTIVE;return 0;
}
/* DONE is immutable and is the last producer access to this result. A second
 * complete lookup after observing DONE rejects a payload copied across that
 * publication. No reader races an ACK: command_busy covers this entire path. */
static int read_completion(struct ap_session *s,int pidfd,u64 seq,u64 operation,struct ap_command_result *r) {
    if(!r || !seq || pidfd<0)return invalid();
    u32 key=ap_command_slot(seq);struct ap_pending_command *p=&s->pending[key];
    if(p->state!=AP_SLOT_ACTIVE || p->submitted.command!=seq || p->submitted.operation!=operation)return invalid();
    struct ap_task_command active;
    if(bpf_map_lookup_elem(s->tasks,&pidfd,&active))return -1;
    if(memcmp(&active,&p->submitted,sizeof(active)))return invalid();
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
/* Retain the receipt before clearing the exact task command. Clearing makes
 * this task available for a different free slot, never this unacknowledged one. */
static int collect(struct ap_session *s,int pidfd,const struct ap_command_result *r) {
    struct ap_pending_command *p=&s->pending[ap_command_slot(r->command)];
    p->receipt=*r;p->state=AP_SLOT_DISARMING;
    struct ap_task_command cleared={.provider=s->incarnation},observed;
    if(bpf_map_update_elem(s->tasks,&pidfd,&cleared,BPF_EXIST))return quarantine(p);
    if(bpf_map_lookup_elem(s->tasks,&pidfd,&observed))return quarantine(p);
    if(memcmp(&cleared,&observed,sizeof(cleared))) { errno=EPROTO;return quarantine(p); }
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
       observed.identity.provider!=s->incarnation || !observed.identity.object || !observed.identity.namespace) {
        errno=primary?primary_errno:EPROTO;goto done;
    }
    if(operation==AP_MATCH && ((expected.object && memcmp(&expected,&observed.identity,sizeof(expected))) || !observed.creation)) { invalid();goto done; }
    if(collect(s,pidfd,&observed))goto done;
    *r=observed;rc=0;
done:
    leave_commands(s);return rc;
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
    if(fd_ack_accept(s,p)) { quarantine(p);goto done; }
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
static int add_identifier(struct ap_program_id *out,u32 capacity,u32 *n,u32 kind,int fd) {
    if(fd<0)return 0; /* not yet loaded: no invented ID */
    union { struct bpf_map_info map;struct bpf_prog_info prog;struct bpf_link_info link; } info={0};
    u32 size=kind==0?sizeof(info.map):(kind==1?sizeof(info.prog):sizeof(info.link));
    if(bpf_obj_get_info_by_fd(fd,&info,&size))return -1;
    u32 id=kind==0?info.map.id:(kind==1?info.prog.id:info.link.id);
    if(!id || *n>=capacity) { errno=EOVERFLOW; return -1; }
    out[(*n)++]=(struct ap_program_id){kind,id};return 0;
}
int ap_identifiers(struct ap_session *s,struct ap_program_id *out,u32 capacity,u32 *written) {
    if(!s || !out || !written)return invalid();
    *written=0;
    if(s->object) {
        struct bpf_map *m=NULL;while((m=bpf_object__next_map(s->object,m)))
            if(add_identifier(out,capacity,written,0,bpf_map__fd(m)))return -1;
        struct bpf_program *p=NULL;while((p=bpf_object__next_program(s->object,p)))
            if(add_identifier(out,capacity,written,1,bpf_program__fd(p)))return -1;
    }
    for(u32 i=0;i<s->links_count;i++)if(add_identifier(out,capacity,written,2,bpf_link__fd(s->links[i])))return -1;
    return 0;
}
int ap_close(struct ap_session *s) {
    if(!s)return 0;
    int failed=0;
    for(u32 i=s->links_count;i>0;i--)if(bpf_link__destroy(s->links[i-1]))failed=1;
    if(s->object)bpf_object__close(s->object);
    free(s);
    return failed?-1:0;
}

#include "fd-effects-driver.h"
