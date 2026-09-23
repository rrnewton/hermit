// SPDX-License-Identifier: GPL-2.0
// Unattached source candidate. Requires the external keeper and backend joins
// listed in REPORT.md. Readiness observation is NOT pre-copyout enforcement.
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wmicrosoft-anon-tag"
#include "vmlinux.h"
#pragma clang diagnostic pop
#include "unix-guard.h"
#define SEC(name) __attribute__((section(name), used))
#define __uint(name, val) int (*name)[val]
#define __type(name, val) typeof(val) *name
#define CORE(value) __builtin_preserve_access_index(value)
#define INLINE static __attribute__((always_inline))
static void *(*lookup)(void *, const void *) = (void *)BPF_FUNC_map_lookup_elem;
static long (*update)(void *, const void *, const void *, u64) = (void *)BPF_FUNC_map_update_elem;
static long (*remove_key)(void *, const void *) = (void *)BPF_FUNC_map_delete_elem;
static struct task_struct *(*current_task)(void) = (void *)BPF_FUNC_get_current_task_btf;
static u64 (*socket_cookie)(struct sock *) = (void *)BPF_FUNC_get_socket_cookie;
static u64 (*pid_tgid)(void) = (void *)BPF_FUNC_get_current_pid_tgid;
static void *(*task_storage)(void *, struct task_struct *, void *, u64) = (void *)BPF_FUNC_task_storage_get;
static struct socket *(*sock_from_file)(struct file *) = (void *)BPF_FUNC_sock_from_file;
static long (*ring_output)(void *, void *, u64, u64) = (void *)BPF_FUNC_ringbuf_output;
static void (*spin_lock)(struct bpf_spin_lock *) = (void *)BPF_FUNC_spin_lock;
static void (*spin_unlock)(struct bpf_spin_lock *) = (void *)BPF_FUNC_spin_unlock;
struct ug_allocator { struct bpf_spin_lock lock; u32 reserved; u64 next_generation; };
struct { __uint(type, BPF_MAP_TYPE_ARRAY); __uint(max_entries, 1);
    __type(key, u32); __type(value, struct ug_config); } ug_config SEC(".maps");
struct { __uint(type, BPF_MAP_TYPE_ARRAY); __uint(max_entries, 1);
    __type(key, u32); __type(value, struct ug_status); } ug_status SEC(".maps");
struct { __uint(type, BPF_MAP_TYPE_ARRAY); __uint(max_entries, 1);
    __type(key, u32); __type(value, struct ug_allocator); } ug_allocator SEC(".maps");
/* At most the first policy denial and first internal fault are published. No
 * telemetry stream competes for space; sticky records remain the authority. */
struct { __uint(type, BPF_MAP_TYPE_RINGBUF); __uint(max_entries, 4096); } ug_events SEC(".maps");
struct { __uint(type, BPF_MAP_TYPE_TASK_STORAGE); __uint(map_flags, BPF_F_NO_PREALLOC);
    __type(key, int); __type(value, struct ug_task); } ug_tasks SEC(".maps");
struct { __uint(type, BPF_MAP_TYPE_HASH); __uint(max_entries, UG_MAX_LIVE_SOCKETS);
    __type(key, u64); __type(value, struct ug_socket); } ug_sockets SEC(".maps");
struct { __uint(type, BPF_MAP_TYPE_HASH); __uint(max_entries, UG_MAX_LIVE_NAMESPACES);
    __type(key, u64); __type(value, struct ug_namespace); } ug_namespaces SEC(".maps");
struct { __uint(type, BPF_MAP_TYPE_TASK_STORAGE); __uint(map_flags, BPF_F_NO_PREALLOC);
    __type(key, int); __type(value, struct ug_birth); } ug_births SEC(".maps");
struct { __uint(type, BPF_MAP_TYPE_HASH); __uint(max_entries, UG_MAX_INITIAL_TASKS);
    __type(key, u64); __type(value, struct ug_initial_task); } ug_initial_tasks SEC(".maps");
struct { __uint(type, BPF_MAP_TYPE_TASK_STORAGE); __uint(map_flags, BPF_F_NO_PREALLOC);
    __type(key, int); __type(value, struct ug_probe); } ug_probes SEC(".maps");
INLINE struct ug_status *status(void) { u32 key = 0; return lookup(&ug_status, &key); }
INLINE struct ug_config *ug_config_value(void) { u32 key = 0; return lookup(&ug_config, &key); }
/* Every status writer and the userspace BPF_F_LOCK reader use this SAME lock.
 * Helper calls, CORE reads and ring output occur only outside locked regions. */
INLINE int status_fault(u64 bit) {
    struct ug_status *s = status();
    if (!s) return 0;
    int first = 0;
    spin_lock(&s->lock);
    s->faults |= bit;
    if (!s->first_outcome) { s->first_outcome = UG_EVENT_INTERNAL; first = 1; }
    spin_unlock(&s->lock);
    return first;
}
INLINE u64 status_faults(void) {
    struct ug_status *s = status();
    if (!s) return UG_MAP_FAILURE;
    spin_lock(&s->lock);
    u64 result = s->faults;
    spin_unlock(&s->lock);
    return result;
}
INLINE void notify(u32 kind) {
    struct ug_config *c = ug_config_value();
    struct ug_event event = { .incarnation = c ? c->incarnation : 0, .kind = kind };
    if (ring_output(&ug_events, &event, sizeof(event), BPF_RB_FORCE_WAKEUP)) {
        status_fault(UG_EVENT_FAILURE);
        /* No recursive ring retry. The independent monitor also reads status
         * periodically, so all-blocked guests do not depend on this wake. */
    }
}
INLINE void fault(u64 bit) {
    if (status_fault(bit)) notify(UG_EVENT_INTERNAL);
}
enum ug_count { UG_SOCKETS, UG_DESCENDANTS, UG_NAMESPACES };
INLINE void status_count(enum ug_count which, int add) {
    struct ug_status *s = status();
    if (!s) return;
    u64 *count = which == UG_SOCKETS ? &s->live_sockets :
                 which == UG_DESCENDANTS ? &s->live_descendants : &s->live_namespaces;
    u64 problem = 0;
    spin_lock(&s->lock);
    if (add) {
        if (*count == ~0ULL) problem = UG_COUNT_OVERFLOW;
        else ++*count;
    } else {
        if (!*count) problem = UG_COUNT_UNDERFLOW;
        else --*count;
    }
    int first = 0;
    if (problem) {
        s->faults |= problem;
        if (!s->first_outcome) { s->first_outcome = UG_EVENT_INTERNAL; first = 1; }
    }
    spin_unlock(&s->lock);
    if (first) notify(UG_EVENT_INTERNAL);
}
INLINE u64 caller(void) {
    struct ug_task *task = task_storage(&ug_tasks, current_task(), 0, 0);
    if (!task) return 0;
    struct ug_config *c = ug_config_value();
    if (!c || !c->incarnation || c->policy != UG_DENY || c->abi_version != UG_ABI_VERSION ||
        task->incarnation != c->incarnation) {
        fault(UG_BAD_TASK); return ~0ULL; /* present-but-invalid remains in scope */
    }
    if (!task->descendant) {
        u64 key = task->initial_registration;
        struct ug_initial_task *initial = lookup(&ug_initial_tasks, &key);
        if (!ug_initial_membership_matches(task, initial)) {
            fault(UG_BAD_INITIAL_TASK); return ~0ULL;
        }
    } else if (task->initial_registration) {
        fault(UG_BAD_INITIAL_TASK); return ~0ULL;
    }
    return task->incarnation;
}
INLINE int is_unix(struct sock *sk) { return sk && CORE(sk->__sk_common.skc_family) == UG_AF_UNIX; }
/* Called only while the kernel hook's caller holds a real reference to sk.
 * The map does not prolong lifetime. __sk_free removes it before pointer reuse. */
INLINE struct ug_socket *owned(struct sock *sk) {
    if (!sk) return 0;
    u64 key = (u64)sk;
    struct ug_socket *o = lookup(&ug_sockets, &key);
    if (o && (!o->cookie || o->cookie != (u64)CORE(sk->__sk_common.skc_cookie.counter))) {
        fault(UG_COOKIE_MISMATCH);
        /* Keep the touched association in scope so finish rejects it as an
         * internal identity failure; never reinterpret it as foreign/absent. */
    }
    return o;
}
INLINE void deny_record(enum ug_hook hook, enum ug_reason reason,
                        const struct ug_socket *source, const struct ug_socket *peer) {
    struct ug_status *s = status();
    if (!s) return;
    struct ug_config *c = ug_config_value();
    struct ug_probe *probe = task_storage(&ug_probes, current_task(), 0, 0);
    struct ug_denial denial = {
        .phase = 2,
        .incarnation = c ? c->incarnation : 0,
        .pid_tgid = pid_tgid(),
        .task_start = CORE(current_task()->start_boottime),
        .source_generation = source ? source->generation : 0,
        .peer_generation = peer ? peer->generation : 0,
        .hook = hook, .reason = reason,
        .injection = probe && c && ug_probe_matches(probe, c->incarnation, probe->sequence)
                     ? probe->sequence : 0,
    };
    u64 outcome = reason == UG_INTERNAL ? UG_EVENT_INTERNAL : UG_EVENT_DENIAL;
    int first = 0;
    spin_lock(&s->lock);
    if (!s->first_denial.phase) s->first_denial = denial;
    if (!s->first_outcome) { s->first_outcome = outcome; first = 1; }
    spin_unlock(&s->lock);
    if (first) notify(outcome);
}
INLINE int finish(enum ug_hook hook, enum ug_reason reason, u64 task,
                   const struct ug_socket *source, const struct ug_socket *peer) {
    /* Global failures affect only a cohort operation or a referenced owned
     * endpoint. Unrelated outside tasks and sockets remain untouched. */
    if ((task || source || peer) && status_faults()) reason = UG_INTERNAL;
    if (reason != UG_ALLOWED) deny_record(hook, reason, source, peer);
    return ug_result(0, reason);
}
INLINE int source_check(struct socket *sock, enum ug_hook hook, int prior) {
    if (prior) return prior;
    struct sock *sk = sock ? CORE(sock->sk) : 0;
    if (!is_unix(sk)) return 0;
    u64 task = caller();
    struct ug_socket *o = owned(sk);
    return finish(hook, ug_source_decision(task, o ? o->incarnation : 0), task, o, 0);
}
INLINE int file_check(struct file *file, enum ug_hook hook, int prior) {
    if (prior) return prior;
    return source_check(sock_from_file(file), hook, 0);
}
INLINE u64 next_generation(void) {
    u32 allocator_key = 0;
    struct ug_allocator *allocator = lookup(&ug_allocator, &allocator_key);
    if (!allocator) { fault(UG_MAP_FAILURE); return 0; }
    u64 generation = 0;
    spin_lock(&allocator->lock);
    if (allocator->next_generation != ~0ULL)
        generation = ++allocator->next_generation;
    spin_unlock(&allocator->lock);
    if (!generation) fault(UG_ID_EXHAUSTED);
    return generation;
}
INLINE int enroll(struct sock *sk, u64 task) {
    if (!task || !is_unix(sk)) return 0;
    if (owned(sk)) { fault(UG_BAD_CREATION); return -UG_EIO; }
    if (status_faults()) return -UG_EIO;
    u64 generation = next_generation();
    if (!generation) return -UG_EIO;
    struct ug_socket fresh = { .incarnation = task, .generation = generation,
                              .cookie = socket_cookie(sk) };
    if (!fresh.cookie) { fault(UG_COOKIE_MISMATCH); return -UG_EIO; }
    u64 key = (u64)sk;
    if (update(&ug_sockets, &key, &fresh, BPF_NOEXIST)) { fault(UG_MAP_FAILURE); return -UG_EIO; }
    status_count(UG_SOCKETS, 1);
    return 0;
}
/* The map holds identity only, never a namespace reference. setup_net entry
 * precedes every pernet initializer and namespace tree/list publication. */
INLINE struct ug_namespace *namespace_owned(struct net *net) {
    if (!net) return 0;
    u64 key = (u64)net;
    struct ug_namespace *ns = lookup(&ug_namespaces, &key);
    if (ns && ns->phase == UG_NAMESPACE_LIVE &&
        (!ns->cookie || ns->cookie != CORE(net->net_cookie)))
        fault(UG_BAD_NAMESPACE);
    return ns; /* retain in-scope identity on mismatch */
}
INLINE void namespace_remove(struct net *net, u64 generation) {
    u64 key = (u64)net;
    struct ug_namespace *ns = namespace_owned(net);
    if (!ns || !generation || ns->generation != generation ||
        (ns->phase == UG_NAMESPACE_LIVE &&
         (!ns->cookie || ns->cookie != CORE(net->net_cookie)))) {
        fault(UG_BAD_NAMESPACE); return;
    }
    if (remove_key(&ug_namespaces, &key)) { fault(UG_MAP_FAILURE); return; }
    status_count(UG_NAMESPACES, 0);
}
INLINE struct ug_birth *birth_command(void) {
    struct ug_birth *b = task_storage(&ug_births, current_task(), 0, 0);
    if (!b) return 0;
    struct ug_config *c = ug_config_value();
    if (!c || !c->incarnation || c->abi_version != UG_ABI_VERSION ||
        c->policy != UG_DENY || b->incarnation != c->incarnation || !b->sequence) {
        fault(UG_BAD_BIRTH); return 0;
    }
    return b;
}
SEC("fentry/copy_net_ns") int namespace_copy_enter(u64 *ctx) {
    if (!(ctx[0] & UG_CLONE_NEWNET)) return 0;
    u64 task = caller();
    struct ug_birth *b = birth_command();
    /* Only a positively registered guest can mint an automatic command. The
     * outside startup parent must be armed through its held pidfd beforehand. */
    if (task) {
        if (status_faults()) return 0;
        if (b && b->in_copy) { fault(UG_BAD_BIRTH); return 0; }
        if (b && b->phase != UG_BIRTH_COMMITTED && b->phase != UG_BIRTH_FAILED) {
            fault(UG_BAD_BIRTH); return 0;
        }
        u64 sequence = next_generation();
        if (!sequence) return 0;
        struct ug_birth fresh = {
            .incarnation = task, .sequence = sequence, .phase = UG_BIRTH_ARMED,
        };
        if (!b) b = task_storage(&ug_births, current_task(), &fresh,
                                 BPF_LOCAL_STORAGE_GET_F_CREATE);
        else *b = fresh;
        if (!b) { fault(UG_MAP_FAILURE); return 0; }
    }
    if (!b) return 0; /* unrelated outside namespace creation */
    if (!task && !b->in_copy &&
        (b->phase == UG_BIRTH_COMMITTED || b->phase == UG_BIRTH_FAILED)) return 0;
    if (__sync_val_compare_and_swap(&b->phase, UG_BIRTH_ARMED, UG_BIRTH_COPY)
        != UG_BIRTH_ARMED || b->in_copy || b->object || b->generation || b->cookie) {
        fault(UG_BAD_BIRTH); return 0;
    }
    b->in_copy = 1;
    return 0;
}
SEC("fentry/setup_net") int namespace_setup_enter(u64 *ctx) {
    struct ug_birth *b = birth_command();
    if (!b || !b->in_copy || b->phase != UG_BIRTH_COPY) return 0;
    struct net *net = (struct net *)ctx[0];
    u64 generation = next_generation();
    if (!net || !generation || b->object) { fault(UG_BAD_BIRTH); return 0; }
    /* This is before setup_net assigns net_cookie. The unique generation is
     * already live, and the actual kernel cookie is bound on successful exit. */
    struct ug_namespace ns = {
        .incarnation = b->incarnation, .generation = generation,
        .phase = UG_NAMESPACE_INITIALIZING,
    };
    u64 key = (u64)net;
    if (update(&ug_namespaces, &key, &ns, BPF_NOEXIST)) {
        fault(UG_MAP_FAILURE); return 0;
    }
    status_count(UG_NAMESPACES, 1);
    b->object = key; b->generation = generation;
    b->phase = UG_BIRTH_INITIALIZING;
    return 0;
}
SEC("fexit/setup_net") int namespace_setup_exit(u64 *ctx) {
    struct ug_birth *b = birth_command();
    if (!b || !b->in_copy || b->phase != UG_BIRTH_INITIALIZING) return 0;
    struct net *net = (struct net *)ctx[0];
    struct ug_namespace *ns = namespace_owned(net);
    if (!ns || b->object != (u64)net || ns->generation != b->generation ||
        ns->phase != UG_NAMESPACE_INITIALIZING) { fault(UG_BAD_BIRTH); return 0; }
    if ((s32)ctx[1]) {
        /* setup_net has already undone its initialized pernet operations and
         * waited for RCU. copy_net_ns frees this failed object without __put_net. */
        namespace_remove(net, b->generation);
        b->object = 0; b->cookie = 0; b->phase = UG_BIRTH_FAILED;
        return 0;
    }
    u64 cookie = CORE(net->net_cookie);
    if (!cookie) { fault(UG_BAD_NAMESPACE); return 0; }
    ns->cookie = cookie; ns->phase = UG_NAMESPACE_LIVE;
    b->cookie = cookie; b->phase = UG_BIRTH_READY;
    return 0;
}
SEC("fexit/copy_net_ns") int namespace_copy_exit(u64 *ctx) {
    if (!(ctx[0] & UG_CLONE_NEWNET)) return 0;
    struct ug_birth *b = birth_command();
    if (!b || !b->in_copy) return 0;
    b->in_copy = 0;
    u64 returned = ctx[3];
    if (returned >= (u64)-4095) {
        /* Allocation/preinit/lock failures never entered setup_net. A failed
         * setup already removed its provisional association above. */
        if ((b->phase != UG_BIRTH_COPY && b->phase != UG_BIRTH_FAILED) || b->object)
            fault(UG_BAD_BIRTH);
        b->phase = UG_BIRTH_FAILED;
        return 0;
    }
    struct ug_namespace *ns = namespace_owned((struct net *)returned);
    if (!returned || !ns || b->phase != UG_BIRTH_READY || b->object != returned ||
        ns->incarnation != b->incarnation || ns->generation != b->generation ||
        ns->cookie != b->cookie) { fault(UG_BAD_BIRTH); return 0; }
    b->phase = UG_BIRTH_COMMITTED;
    return 0;
}
SEC("fentry/__put_net") int namespace_last_reference(u64 *ctx) {
    struct net *net = (struct net *)ctx[0];
    struct ug_namespace *ns = namespace_owned(net);
    if (!ns) return 0;
    /* put_net reaches this only after the ordinary namespace refcount reaches
     * zero (stronger than active-use count zero). No task or userspace socket
     * can acquire a new ordinary reference. Passive kernel/RCU
     * references may remain: this is not a claim of completed memory free. */
    if (ns->phase != UG_NAMESPACE_LIVE ||
        CORE(net->ns.__ns_ref.refs.counter) != 0) { fault(UG_BAD_NAMESPACE); return 0; }
    namespace_remove(net, ns->generation);
    return 0;
}

/* Prior stacked BPF/LSM nonzero results are returned before state mutation. */
SEC("lsm/socket_post_create") int socket_created(u64 *ctx) {
    int prior = (s32)ctx[5]; if (prior) return prior;
    if ((s32)ctx[1] != UG_AF_UNIX) return 0;
    struct socket *sock = (struct socket *)ctx[0];
    struct sock *sk = sock ? CORE(sock->sk) : 0;
    if (!sk) { fault(UG_BAD_CREATION); return -UG_EIO; }
    u64 task = caller();
    struct ug_namespace *ns = namespace_owned(CORE(sk->__sk_common.skc_net.net));
    enum ug_reason reason = ug_creation_decision(task, ns ? ns->incarnation : 0,
                                                (s32)ctx[4] != 0);
    /* INITIALIZING is guarded too: no pernet initializer may plant a foreign
     * Unix endpoint before setup_net returns. A kernel creation there remains
     * an explicit internal unsupported effect, never cohort provenance. */
    if ((task || ns) && (!ns || ns->phase != UG_NAMESPACE_LIVE)) {
        if (reason == UG_ALLOWED) reason = UG_INTERNAL;
    }
    int result = finish(UG_NAMESPACE_CREATE, reason, task, 0, 0);
    if (result) return result;
    return enroll(sk, task);
}
SEC("lsm/task_alloc") int task_created(u64 *ctx) {
    int prior = (s32)ctx[2]; if (prior) return prior;
    u64 task = caller(); if (!task) return 0;
    if (status_faults()) return -UG_EIO;
    struct ug_task fresh = { .incarnation = task, .descendant = 1 };
    struct task_struct *child = (struct task_struct *)ctx[0];
    if (task_storage(&ug_tasks, child, 0, 0)) { fault(UG_BAD_TASK); return -UG_EIO; }
    if (!task_storage(&ug_tasks, child, &fresh, BPF_LOCAL_STORAGE_GET_F_CREATE)) {
        fault(UG_MAP_FAILURE); return -UG_EIO;
    }
    status_count(UG_DESCENDANTS, 1);
    return 0;
}
SEC("lsm/task_free") int task_freed(u64 *ctx) {
    struct ug_task *task = task_storage(&ug_tasks, (struct task_struct *)ctx[0], 0, 0);
    if (!task) return 0;
    if (task->descendant) {
        status_count(UG_DESCENDANTS, 0);
        task->descendant = 0;
    } else {
        u64 key = task->initial_registration;
        struct ug_initial_task *initial = lookup(&ug_initial_tasks, &key);
        if (!key || !initial || initial->incarnation != task->incarnation ||
            __sync_val_compare_and_swap(&initial->phase, UG_INITIAL_LIVE,
                                        UG_INITIAL_TERMINAL) != UG_INITIAL_LIVE)
            fault(UG_BAD_INITIAL_TASK);
        /* Keep the terminal row until the keeper also observes the exact held
         * pidfd terminal. No missing row means success and no numeric PID reuse. */
    }
    return 0;
}
SEC("fentry/__sk_free") int socket_freed(u64 *ctx) {
    struct sock *sk = (struct sock *)ctx[0];
    struct ug_socket *o = owned(sk); if (!o) return 0;
    u64 key = (u64)sk;
    if (remove_key(&ug_sockets, &key)) { fault(UG_MAP_FAILURE); return 0; }
    status_count(UG_SOCKETS, 0);
    return 0;
}
SEC("lsm/unix_find") int pathname_peer(u64 *ctx) {
    int prior = (s32)ctx[3]; if (prior) return prior;
    u64 task = caller();
    struct ug_socket *peer = owned((struct sock *)ctx[1]);
    return finish(UG_PATH_PEER, ug_path_decision(task, peer ? peer->incarnation : 0), task, 0, peer);
}
SEC("lsm/unix_stream_connect") int stream_peer(u64 *ctx) {
    int prior = (s32)ctx[3]; if (prior) return prior;
    u64 task = caller();
    struct ug_socket *source = owned((struct sock *)ctx[0]);
    struct ug_socket *peer = owned((struct sock *)ctx[1]);
    int result = finish(UG_STREAM_PEER, ug_peer_decision(task,
        source ? source->incarnation : 0, peer ? peer->incarnation : 0), task, source, peer);
    if (result) return result;
    /* The server child is allocated before the hook and not published yet.
     * Same-cohort-only success makes its creator/listener membership equal. */
    return enroll((struct sock *)ctx[2], task);
}
SEC("lsm/unix_may_send") int dgram_peer(u64 *ctx) {
    int prior = (s32)ctx[2]; if (prior) return prior;
    struct socket *a = (struct socket *)ctx[0], *b = (struct socket *)ctx[1];
    u64 task = caller();
    struct ug_socket *source = owned(CORE(a->sk)), *peer = owned(CORE(b->sk));
    return finish(UG_DGRAM_PEER, ug_peer_decision(task,
        source ? source->incarnation : 0, peer ? peer->incarnation : 0), task, source, peer);
}
SEC("lsm/socket_socketpair") int socket_pair(u64 *ctx) {
    int prior = (s32)ctx[2]; if (prior) return prior;
    struct socket *a = (struct socket *)ctx[0], *b = (struct socket *)ctx[1];
    if (!is_unix(CORE(a->sk))) return 0;
    u64 task = caller();
    struct ug_socket *source = owned(CORE(a->sk)), *peer = owned(CORE(b->sk));
    return finish(UG_PAIR, ug_peer_decision(task,
        source ? source->incarnation : 0, peer ? peer->incarnation : 0), task, source, peer);
}
#define SOURCE_HOOK(name, argc, tag) \
    SEC("lsm/" #name) int guard_##name(u64 *ctx) { \
        return source_check((struct socket *)ctx[0], tag, (s32)ctx[argc]); \
    }
SOURCE_HOOK(socket_bind, 3, UG_BIND)
SOURCE_HOOK(socket_connect, 3, UG_CONNECT)
SOURCE_HOOK(socket_listen, 2, UG_LISTEN)
SOURCE_HOOK(socket_accept, 2, UG_ACCEPT)
SOURCE_HOOK(socket_sendmsg, 3, UG_SEND)
SOURCE_HOOK(socket_recvmsg, 4, UG_RECV)
SOURCE_HOOK(socket_getsockname, 1, UG_NAME)
SOURCE_HOOK(socket_getpeername, 1, UG_NAME)
SOURCE_HOOK(socket_getsockopt, 3, UG_OPTION)
SOURCE_HOOK(socket_setsockopt, 3, UG_OPTION)
SOURCE_HOOK(socket_shutdown, 2, UG_SHUTDOWN)
SEC("lsm/file_permission") int guard_file_permission(u64 *ctx) {
    return file_check((struct file *)ctx[0], UG_FILE_READ_WRITE, (s32)ctx[2]);
}
SEC("lsm/file_ioctl") int guard_file_ioctl(u64 *ctx) {
    return file_check((struct file *)ctx[0], UG_FILE_IOCTL, (s32)ctx[3]);
}
SEC("lsm/file_ioctl_compat") int guard_file_ioctl_compat(u64 *ctx) {
    return file_check((struct file *)ctx[0], UG_FILE_IOCTL, (s32)ctx[3]);
}
SEC("lsm/file_fcntl") int guard_file_fcntl(u64 *ctx) {
    return file_check((struct file *)ctx[0], UG_FILE_FCNTL, (s32)ctx[3]);
}
SEC("lsm/file_receive") int guard_file_receive(u64 *ctx) {
    return file_check((struct file *)ctx[0], UG_FILE_RECEIVE, (s32)ctx[1]);
}
/* Observes the actual socket reached by vfs_poll/ep_item_poll. This does NOT
 * suppress native copyout or provide private staging. It cannot activate the
 * Deny capability without the shared backend's pre-exposure completion join. */
INLINE int readiness_observe(struct socket *sock) {
    struct sock *sk = sock ? CORE(sock->sk) : 0;
    if (!is_unix(sk)) return 0;
    u64 task = caller();
    struct ug_socket *source = owned(sk);
    if (!task && !source) return 0;
    struct ug_probe *probe = task_storage(&ug_probes, current_task(), 0, 0);
    if (task) {
        if (!probe || !ug_probe_matches(probe, task, probe->sequence)) {
            fault(UG_BAD_PROBE);
        } else {
            if (__sync_fetch_and_add(&probe->observations, 1) == ~0ULL)
                fault(UG_BAD_PROBE);
        }
    }
    int result = finish(UG_POLL,
        ug_source_decision(task, source ? source->incarnation : 0), task, source, 0);
    if (result && probe) __sync_fetch_and_or(&probe->denied, 1);
    return 0; /* fentry is observation, never claim native return modification */
}
SEC("fentry/unix_poll") int stream_readiness(u64 *ctx) {
    return readiness_observe((struct socket *)ctx[1]);
}
SEC("fentry/unix_dgram_poll") int packet_readiness(u64 *ctx) {
    return readiness_observe((struct socket *)ctx[1]);
}

char LICENSE[] SEC("license") = "GPL";
