// SPDX-License-Identifier: GPL-2.0
// Read-only prototype. No SK_STORAGE, policy mutation or guest option charge.
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wmicrosoft-anon-tag"
#include "vmlinux.h"
#pragma clang diagnostic pop
#include "provider.h"
#include "provider-abi.h"
#include "auth-diagnostic.h"
#include "setter-level.h"
#define SEC(name) __attribute__((section(name), used))
#define __uint(name, val) int (*name)[val]
#define __type(name, val) typeof(val) *name
#define CORE(value) __builtin_preserve_access_index(value)
#define INLINE static __attribute__((always_inline))
static void *(*lookup)(void *, const void *)=(void *)BPF_FUNC_map_lookup_elem;
static long (*update)(void *, const void *, const void *, u64)=(void *)BPF_FUNC_map_update_elem;
static long (*remove_key)(void *, const void *)=(void *)BPF_FUNC_map_delete_elem;
static struct task_struct *(*current_task)(void)=(void *)BPF_FUNC_get_current_task_btf;
static u64 (*socket_cookie)(struct sock *)=(void *)BPF_FUNC_get_socket_cookie;
static u64 (*pid_tgid)(void)=(void *)BPF_FUNC_get_current_pid_tgid;
static struct tcp_sock *(*to_tcp)(struct sock *)=(void *)BPF_FUNC_skc_to_tcp_sock;
static void *(*task_storage)(void *, struct task_struct *, void *, u64)=(void *)BPF_FUNC_task_storage_get;
struct ap_object { u64 object, creation, listener, cookie; };
struct ap_listener { u64 generation, epoch, active, unresolved, retired; };
struct ap_invocation_key { u64 sock, request, task, start; };
struct ap_clone_call { u64 listener, generation, epoch, overlap; struct ap_raw_state before; };
struct ap_setter_call { u64 listener, command, before, after, authorized; };
#define ARRAY(name, value_type, limit) struct { __uint(type,BPF_MAP_TYPE_ARRAY); __uint(max_entries,limit); __type(key,u32); __type(value,value_type); } name SEC(".maps")
#define HASH(name, key_type, value_type, limit) struct { __uint(type,BPF_MAP_TYPE_HASH); __uint(max_entries,limit); __type(key,key_type); __type(value,value_type); } name SEC(".maps")
ARRAY(ap_config_map,struct ap_config,1);
ARRAY(status,struct ap_status,1);
ARRAY(listeners,struct ap_listener,AP_OBJECTS);
ARRAY(events,struct ap_creation,AP_EVENTS);
ARRAY(commands,struct ap_command_result,AP_COMMANDS);
HASH(objects,u64,struct ap_object,AP_OBJECTS);
HASH(clones,struct ap_invocation_key,struct ap_clone_call,AP_CALLS);
HASH(setters,struct ap_invocation_key,struct ap_setter_call,AP_CALLS);
struct { __uint(type,BPF_MAP_TYPE_TASK_STORAGE); __uint(map_flags,BPF_F_NO_PREALLOC);
    __type(key,int); __type(value,struct ap_task_command); } tasks SEC(".maps");
INLINE struct ap_status *stats(void) { u32 k=0; return lookup(&status,&k); }
INLINE void ap_fail(u64 reason) { struct ap_status *s=stats(); if(s)__sync_fetch_and_or(&s->fatal,reason); }
INLINE u64 incarnation(void) { u32 k=0; struct ap_config *c=lookup(&ap_config_map,&k); return c?c->provider:0; }
INLINE struct ap_task_command *raw_command(void) {
    return task_storage(&tasks,current_task(),0,0);
}
INLINE struct ap_task_command *authenticated_command(struct ap_task_command *c) {
    return c && c->provider && c->provider==incarnation()?c:0;
}
INLINE struct ap_task_command *command(void) { return authenticated_command(raw_command()); }
INLINE struct ap_object *object(struct sock *sk) {
    u64 k=(u64)sk;struct ap_object *o=lookup(&objects,&k);
    if(o && (!o->cookie || o->cookie!=(u64)CORE(sk->__sk_common.skc_cookie.counter))) {
        ap_fail(AP_WRONG_IDENTITY);return 0;
    }
    return o;
}
INLINE struct ap_listener *listener(u64 id) { if(!id||id>=AP_OBJECTS)return 0; u32 k=id; return lookup(&listeners,&k); }
INLINE struct ap_creation *creation(u64 seq) { if(!seq||seq>=AP_EVENTS)return 0; u32 k=seq; return lookup(&events,&k); }
INLINE struct ap_command_result *result(u64 ticket) {
    if(!ticket)return 0;
    u32 k=ap_command_slot(ticket);
    struct ap_command_result *r=lookup(&commands,&k);
    return r && r->command==ticket?r:0;
}
INLINE struct ap_command_result *claim_result(const struct ap_task_command *c) {
    struct ap_command_result *r=result(c->command);
    if(!ap_command_reservation_matches(c,r,ap_command_slot(c->command)) ||
       __sync_val_compare_and_swap(&r->phase,AP_COMMAND_READY,AP_COMMAND_RUNNING)!=AP_COMMAND_READY) {
        ap_fail(AP_DUPLICATE);return 0;
    }
    r->task=pid_tgid();r->start_boottime=CORE(current_task()->start_boottime);
    return r;
}
/* This publication is the producer's FINAL access to the result cell. Userspace
 * may acknowledge and reuse it after observing DONE and retaining its receipt. */
INLINE void publish_result(struct ap_command_result *r) {
    if(__sync_val_compare_and_swap(&r->phase,AP_COMMAND_RUNNING,AP_COMMAND_DONE)!=AP_COMMAND_RUNNING)
        ap_fail(AP_BAD_COMMAND);
}
INLINE struct ap_invocation_key invocation(struct sock *sk,u64 extra) {
    struct ap_invocation_key k={.sock=(u64)sk,.request=extra,.task=pid_tgid(),.start=CORE(current_task()->start_boottime)};
    return k;
}
INLINE int raw_state(struct sock *sk,struct ap_raw_state *r) {
    struct tcp_sock *tp=to_tcp(sk);
    if(!tp || CORE(sk->__sk_common.skc_family)!=AP_AF_INET || CORE(sk->sk_protocol)!=AP_IPPROTO_TCP) {
        ap_fail(AP_NOT_TCP4); return -1;
    }
    r->receive_timeout_ticks=CORE(sk->sk_rcvtimeo);
    r->send_timeout_ticks=CORE(sk->sk_sndtimeo);
    r->lowat=CORE(sk->sk_rcvlowat);
    r->receive_buffer=CORE(sk->sk_rcvbuf);
    r->peek_offset=CORE(sk->sk_peek_off);
    r->socket_option_memory=CORE(sk->sk_omem_alloc.counter);
    r->window_clamp=CORE(tp->window_clamp);
    r->userlocks=CORE(sk->sk_userlocks);
    r->scaling_ratio=CORE(tp->scaling_ratio);
    r->tcp_state=CORE(sk->__sk_common.skc_state);
    r->child_spin_locked=CORE(sk->sk_lock.slock.rlock.raw_lock.locked);
    return 0;
}
INLINE struct ap_identity identity(struct sock *sk,u64 id) {
    struct ap_identity i={.provider=incarnation(),.object=id,.namespace=CORE(sk->__sk_common.skc_net.net->ns.inum)};
    return i;
}
INLINE u64 allocate_object(void) {
    struct ap_status *s=stats(); if(!s)return 0;
    u64 id=__sync_fetch_and_add(&s->next_object,1)+1;
    if(id>=AP_OBJECTS) { ap_fail(AP_CAPACITY); return 0; } return id;
}
/* A real held descriptor invokes SO_COOKIE in a TASK_STORAGE-authenticated task.
 * No user-provided numeric fd or pointer is accepted as kernel identity. */
SEC("fexit/sk_getsockopt")
int held_fd_probe(u64 *ctx) {
    struct ap_task_command *c=command();
    if(!c || (c->operation!=AP_ENROLL && c->operation!=AP_MATCH))return 0;
    if(ap_socket_semantic_level(ctx[1])!=AP_SOL_SOCKET || (s32)ctx[2]!=AP_SO_COOKIE) { ap_fail(AP_BAD_COMMAND); return 0; }
    struct ap_command_result *r=claim_result(c);
    if(!r)return 0;
    r->returned=ap_getsockopt_result(ctx);
    if(r->returned) { publish_result(r);return 0; }
    struct sock *sk=(struct sock *)ctx[0];
    if(raw_state(sk,&r->state))return 0;
    struct ap_object *o=object(sk);
    if(c->operation==AP_ENROLL) {
        if(o || r->state.tcp_state!=AP_TCP_CLOSE) { ap_fail(AP_UNSUPPORTED_STATE); return 0; }
        u64 id=allocate_object(); if(!id)return 0;
        struct ap_listener *l=listener(id); if(!l) { ap_fail(AP_MAP_FAILURE); return 0; }
        l->generation=c->generation_before;
        struct ap_object fresh={.object=id,.listener=id,.cookie=CORE(sk->__sk_common.skc_cookie.counter)}; u64 key=(u64)sk;
        if(update(&objects,&key,&fresh,BPF_NOEXIST)) { ap_fail(AP_MAP_FAILURE); return 0; }
        o=object(sk);
    }
    if(!o || (c->operation==AP_MATCH && ((c->expected_object && o->object!=c->expected_object) || !o->creation))) {
        ap_fail(AP_WRONG_IDENTITY); return 0;
    }
    r->identity=identity(sk,o->object); r->creation=o->creation;
    r->cookie=CORE(sk->__sk_common.skc_cookie.counter);
    if(!r->cookie) { ap_fail(AP_WRONG_IDENTITY); return 0; }
    if(c->operation==AP_MATCH) {
        struct ap_creation *e=creation(o->creation);
        if(!e || !(e->phase&AP_CREATED) || e->phase&AP_RETIRED || e->child.object!=o->object || e->cookie_at_creation!=o->cookie) { ap_fail(AP_WRONG_IDENTITY); return 0; }
        __sync_fetch_and_or(&e->phase,AP_MATCHED);
        struct ap_status *s=stats(); if(s)__sync_fetch_and_add(&s->matched,1);
    }
    publish_result(r);return 0;
}
INLINE void save_setter_rejection(const struct ap_task_command *raw,u64 id,u64 generation,s32 level,s32 option,s32 raw_level) {
    u32 key=0;struct ap_setter_rejection *d=lookup(&commands,&key);
    if(!d || __sync_val_compare_and_swap(&d->phase,0,1))return;
    d->task=pid_tgid();d->start_boottime=CORE(current_task()->start_boottime);
    d->incarnation=incarnation();d->object=id;d->generation=generation;
    d->level=level;d->option=option;d->raw_present=raw!=0;d->raw_level=raw_level;
    if(raw)d->raw=*raw;
    d->mismatch=ap_authorization_mismatch(raw,d->incarnation,id,generation,level,option);
    /* Publish after every payload field. No later failure overwrites the first. */
    __sync_val_compare_and_swap(&d->phase,1,2);
}
INLINE int setter_enter(struct sock *sk,s32 level,s32 option,u64 hook,s32 raw_level) {
    struct ap_object *o=object(sk); if(!o || o->creation)return 0;
    struct ap_listener *l=listener(o->object); if(!l)return 0;
    struct ap_task_command *raw=raw_command();
    struct ap_task_command *c=authenticated_command(raw);
    struct ap_setter_call call={.listener=o->object};
    if(c && !ap_authorization_mismatch(c,incarnation(),o->object,l->generation,level,option)) {
        /* Full monotonic ticket plus a READY reservation replaces the old
         * ticket==array-index test. The physical map still has31 normal cells. */
        struct ap_command_result *r=claim_result(c);
        if(!r) { __sync_fetch_and_or(&l->unresolved,AP_BAD_COMMAND);return 0; }
        call.command=c->command; call.before=c->generation_before;
        call.after=c->generation_after; call.authorized=1;
    } else {
        save_setter_rejection(raw,o->object,l->generation,level,option,raw_level);
        __sync_fetch_and_or(&l->unresolved,AP_UNKNOWN_MUTATION); ap_fail(AP_UNKNOWN_MUTATION);
    }
    struct ap_invocation_key k=invocation(sk,hook);
    if(update(&setters,&k,&call,BPF_NOEXIST)) { ap_fail(AP_DUPLICATE); return 0; }
    if(__sync_fetch_and_add(&l->active,1)) { __sync_fetch_and_or(&l->unresolved,AP_MUTATION_OVERLAP); ap_fail(AP_MUTATION_OVERLAP); }
    __sync_fetch_and_add(&l->epoch,1);
    struct ap_status *s=stats(); if(s)__sync_fetch_and_add(&s->setters_entered,1);
    return 0;
}
INLINE int setter_exit(struct sock *sk,s32 returned,u64 hook) {
    struct ap_object *o=object(sk); if(!o || o->creation)return 0;
    struct ap_listener *l=listener(o->object); if(!l)return 0;
    struct ap_invocation_key k=invocation(sk,hook);
    struct ap_setter_call *call=lookup(&setters,&k);
    if(!call || !l->active || call->listener!=o->object) { ap_fail(AP_MISSING); return 0; }
    struct ap_command_result *completed=0;
    if(call->authorized) {
        struct ap_command_result *r=result(call->command);
        if(!r || r->phase!=AP_COMMAND_RUNNING || r->operation!=AP_SETTER ||
           r->task!=k.task || r->start_boottime!=k.start || call->before!=l->generation) { ap_fail(AP_STALE_GENERATION); }
        else {
            r->returned=returned;r->identity=identity(sk,o->object);
            if(!raw_state(sk,&r->state))completed=r;
            if(!returned && !l->unresolved)l->generation=call->after;
        }
    }
    /* Complete every listener/call-map/counter write BEFORE publishing DONE.
     * Do not dereference call after deleting its map entry. */
    __sync_fetch_and_add(&l->epoch,1);
    if(__sync_fetch_and_sub(&l->active,1)!=1) { ap_fail(AP_MUTATION_OVERLAP);completed=0; }
    if(remove_key(&setters,&k)) { ap_fail(AP_MAP_FAILURE);completed=0; }
    struct ap_status *s=stats(); if(s)__sync_fetch_and_add(&s->setters_exited,1);
    if(l->unresolved)completed=0;
    if(completed)publish_result(completed);
    return 0;
}
SEC("fentry/sk_setsockopt") int socket_set_enter(u64 *ctx) { return setter_enter((struct sock *)ctx[0],ap_socket_semantic_level(ctx[1]),ctx[2],1,ctx[1]); }
SEC("fexit/sk_setsockopt") int socket_set_exit(u64 *ctx) { return setter_exit((struct sock *)ctx[0],ap_setsockopt_result(ctx),1); }
SEC("fentry/tcp_setsockopt") int tcp_set_enter(u64 *ctx) { return setter_enter((struct sock *)ctx[0],ctx[1],ctx[2],2,ctx[1]); }
SEC("fexit/tcp_setsockopt") int tcp_set_exit(u64 *ctx) { return setter_exit((struct sock *)ctx[0],ap_setsockopt_result(ctx),2); }
SEC("fentry/tcp_v4_syn_recv_sock")
int clone_enter(u64 *ctx) {
    struct sock *sk=(struct sock *)ctx[0];
    struct ap_object *o=object(sk); if(!o || o->creation)return 0;
    struct ap_listener *l=listener(o->object); if(!l)return 0;
    struct ap_clone_call c={.listener=o->object,.epoch=l->epoch};
    c.generation=l->generation;
    c.overlap=l->active || l->unresolved || l->retired;
    if(raw_state(sk,&c.before))return 0;
    if(c.before.tcp_state!=AP_TCP_LISTEN) { ap_fail(AP_UNSUPPORTED_STATE); return 0; }
    if(c.epoch!=l->epoch)c.overlap=1;
    struct ap_invocation_key k=invocation(sk,ctx[2]);
    if(update(&clones,&k,&c,BPF_NOEXIST)) { ap_fail(AP_DUPLICATE); return 0; }
    struct ap_status *s=stats(); if(s)__sync_fetch_and_add(&s->clone_entries,1);
    return 0;
}
SEC("fexit/tcp_v4_syn_recv_sock")
int clone_exit(u64 *ctx) {
    struct sock *sk=(struct sock *)ctx[0];
    struct ap_object *parent=object(sk); if(!parent || parent->creation)return 0;
    struct ap_invocation_key key=invocation(sk,ctx[2]);
    struct ap_clone_call *call=lookup(&clones,&key);
    struct ap_listener *l=listener(parent->object);
    struct ap_status *s=stats();
    if(!call || !l || !s || call->listener!=parent->object) { ap_fail(AP_MISSING); return 0; }
    struct sock *child=(struct sock *)ctx[AP_CLONE_RESULT_SLOT]; /* seven scalar/pointer arguments */
    if(!child) {
        __sync_fetch_and_add(&s->clone_null_returns,1);
        if(remove_key(&clones,&key))ap_fail(AP_MAP_FAILURE);
        return 0;
    }
    u64 sequence=__sync_fetch_and_add(&s->next_creation,1)+1;
    struct ap_creation *e=creation(sequence);
    u64 child_id=allocate_object();
    if(!e || !child_id) { ap_fail(AP_CAPACITY); return 0; }
    e->sequence=sequence; e->listener=identity(sk,parent->object);
    e->child=identity(child,child_id); e->listener_generation=call->generation;
    e->mutation_epoch_enter=call->epoch; e->listener_before=call->before;
    if(raw_state(child,&e->child_created) || raw_state(sk,&e->listener_after))return 0;
    e->mutation_epoch_exit=l->epoch;
    e->overlap=call->overlap || l->active || l->unresolved || l->retired ||
        call->epoch!=e->mutation_epoch_exit || call->generation!=l->generation;
    if(!e->child_created.child_spin_locked)ap_fail(AP_CHILD_UNLOCKED);
    e->cookie_at_creation=socket_cookie(child);
    if(!e->cookie_at_creation) { ap_fail(AP_WRONG_IDENTITY); return 0; }
    e->local.address_be=CORE(child->__sk_common.skc_rcv_saddr);
    e->local.port_be=__builtin_bswap16(CORE(child->__sk_common.skc_num));
    e->local.family=AP_AF_INET;
    e->peer.address_be=CORE(child->__sk_common.skc_daddr);
    e->peer.port_be=CORE(child->__sk_common.skc_dport);
    e->peer.family=AP_AF_INET;
    if(!e->local.port_be || !e->peer.port_be || e->listener.namespace!=e->child.namespace)ap_fail(AP_ENDPOINT);
    struct ap_object fresh={.object=child_id,.creation=sequence,.listener=parent->object,.cookie=e->cookie_at_creation};
    u64 address=(u64)child;
    if(update(&objects,&address,&fresh,BPF_NOEXIST)) { ap_fail(AP_MAP_FAILURE); return 0; }
    __sync_fetch_and_or(&e->phase,AP_CREATED);
    __sync_fetch_and_add(&s->created,1);
    if(remove_key(&clones,&key))ap_fail(AP_MAP_FAILURE);
    return 0;
}
/* Creation is distinct from successful publication onto the accept queue. */
SEC("fexit/inet_csk_complete_hashdance")
int child_queued(u64 *ctx) {
    struct sock *sk=(struct sock *)ctx[0], *child=(struct sock *)ctx[AP_HASHDANCE_RESULT_SLOT];
    if(!child)return 0;
    struct ap_object *parent=object(sk); if(!parent || parent->creation)return 0;
    struct ap_object *o=object(child);
    if(!o || !o->creation || o->listener!=parent->object) { ap_fail(AP_MISSING); return 0; }
    struct ap_creation *e=creation(o->creation);
    if(!e || !(e->phase&AP_CREATED) || e->phase&AP_RETIRED) { ap_fail(AP_MISSING); return 0; }
    if(__sync_fetch_and_or(&e->phase,AP_QUEUED)&AP_QUEUED)ap_fail(AP_DUPLICATE);
    struct ap_status *s=stats(); if(s)__sync_fetch_and_add(&s->queued,1);
    return 0;
}
/* Before actual destruction/reuse. The retained creation record is not deleted;
 * only the live pointer->generation association is retired. */
SEC("fentry/__sk_free")
int socket_retired(u64 *ctx) {
    struct sock *sk=(struct sock *)ctx[0];
    struct ap_object *o=object(sk); if(!o)return 0;
    if(o->creation) {
        struct ap_creation *e=creation(o->creation);
        if(!e || !(e->phase&AP_CREATED) || e->child.object!=o->object)ap_fail(AP_MISSING);
        else __sync_fetch_and_or(&e->phase,AP_RETIRED);
    } else {
        struct ap_listener *l=listener(o->object); if(!l)ap_fail(AP_MISSING); else l->retired=1;
    }
    u64 key=(u64)sk; if(remove_key(&objects,&key))ap_fail(AP_MAP_FAILURE);
    struct ap_status *s=stats(); if(s)__sync_fetch_and_add(&s->retired,1);
    return 0;
}
char LICENSE[] SEC("license")="GPL";

#include "fd-effects.bpf.h"
