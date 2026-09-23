/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause
 */
#ifndef HERMIT_UNIX_GUARD_H
#define HERMIT_UNIX_GUARD_H
#ifndef __BPF__
#include <stdint.h>
#include <linux/bpf.h>
typedef uint64_t u64;
typedef uint32_t u32;
typedef int32_t s32;
#endif
/* Source candidate only. No loader, attachment or product capability is added. */
#define UG_AF_UNIX 1
#define UG_MAX_LIVE_SOCKETS 4096
#define UG_DENY 1
#define UG_ABI_VERSION 3
#define UG_MAX_LIVE_NAMESPACES 128
#define UG_MAX_INITIAL_TASKS 256
#define UG_CLONE_NEWNET 0x40000000ULL
#define UG_EPERM 1
#define UG_EIO 5

enum ug_reason {
    UG_ALLOWED = 0,
    UG_FOREIGN_SOCKET = 1,
    UG_FOREIGN_CALLER = 2,
    UG_FOREIGN_PEER = 3,
    UG_INTERNAL = 4,
    UG_FOREIGN_NAMESPACE = 5,
};
enum ug_hook {
    UG_PATH_PEER = 1, UG_STREAM_PEER, UG_DGRAM_PEER, UG_PAIR,
    UG_BIND, UG_CONNECT, UG_LISTEN, UG_ACCEPT, UG_SEND, UG_RECV,
    UG_NAME, UG_OPTION, UG_SHUTDOWN, UG_FILE_READ_WRITE, UG_FILE_IOCTL,
    UG_FILE_RECEIVE, UG_FILE_FCNTL, UG_NAMESPACE_CREATE, UG_POLL,
};
enum ug_fault {
    UG_BAD_CONFIG = 1, UG_BAD_TASK = 2, UG_COOKIE_MISMATCH = 4,
    UG_ID_EXHAUSTED = 8, UG_MAP_FAILURE = 16, UG_BAD_CREATION = 32,
    UG_COUNT_UNDERFLOW = 64, UG_EVENT_FAILURE = 128,
    UG_BAD_NAMESPACE = 256, UG_BAD_BIRTH = 512, UG_BAD_PROBE = 1024,
    UG_BAD_INITIAL_TASK = 2048, UG_COUNT_OVERFLOW = 4096,
};
struct ug_config { u64 incarnation; u32 policy, abi_version; };
/* The control plane inserts only the initial stopped task using its held pidfd.
 * Descendants receive membership in task_alloc, before they can execute. */
struct ug_task {
    u64 incarnation, initial_registration; /* zero only for inherited membership */
    u32 descendant, reserved;
};
/* Initial registrations are retained separately from descendant accounting.
 * The loader inserts a STAGED row before linking TASK_STORAGE on the exact
 * stopped task's held pidfd. Partial registration retains that row and pidfd.
 * task_free proves kernel terminal membership; an absent callback is NOT zero. */
enum ug_initial_phase { UG_INITIAL_STAGED = 1, UG_INITIAL_LIVE, UG_INITIAL_TERMINAL };
struct ug_initial_task { u64 incarnation, phase; };
enum ug_birth_phase {
    UG_BIRTH_ARMED = 1, UG_BIRTH_COPY, UG_BIRTH_INITIALIZING,
    UG_BIRTH_READY, UG_BIRTH_COMMITTED, UG_BIRTH_FAILED,
};
/* Only a held-pidfd authenticated creator may be armed from the control plane.
 * The BPF hooks derive object/generation/cookie; userspace supplies none of them.
 * In-cohort copy_net_ns gets an automatically allocated one-use command. */
struct ug_birth {
    u64 incarnation, sequence, phase, in_copy;
    u64 object, generation, cookie;
};
enum ug_namespace_phase { UG_NAMESPACE_INITIALIZING = 1, UG_NAMESPACE_LIVE };
struct ug_namespace { u64 incarnation, generation, cookie, phase; };
enum ug_probe_phase { UG_PROBE_ARMED = 1, UG_PROBE_SUBMITTED, UG_PROBE_COMPLETED };
struct ug_probe {
    u64 incarnation, sequence, phase, observations, denied;
};
struct ug_socket { u64 incarnation, generation, cookie; };
struct ug_denial {
    u64 phase; /* 0 empty, 1 being written, 2 committed; never overwrite */
    u64 incarnation, task_start, pid_tgid;
    u64 source_generation, peer_generation, injection;
    u32 hook, reason;
};
enum ug_event_kind { UG_EVENT_DENIAL = 1, UG_EVENT_INTERNAL = 2 };
struct ug_event { u64 incarnation; u32 kind, reserved; };
struct ug_status {
    struct bpf_spin_lock lock;
    u32 reserved;
    u64 faults, live_sockets, live_descendants, live_namespaces;
    u64 first_outcome; /* atomic ug_event_kind; never replaced by cleanup faults */
    struct ug_denial first_denial;
};
/* Zero is positively outside this provider's cohort. Unresolved identity is a
 * separate internal failure and must never be converted to an external denial. */
static inline enum ug_reason ug_source_decision(u64 caller, u64 socket_owner) {
    if (caller == socket_owner) return UG_ALLOWED;
    return caller ? UG_FOREIGN_SOCKET : UG_FOREIGN_CALLER;
}
static inline enum ug_reason ug_peer_decision(u64 caller, u64 source, u64 peer) {
    enum ug_reason reason = ug_source_decision(caller, source);
    if (reason != UG_ALLOWED) return reason;
    return caller == peer ? UG_ALLOWED : UG_FOREIGN_PEER;
}
/* Path lookup has a referenced peer but no source argument. The earlier socket
 * connect/send hook authenticates that source. This does not authorize any
 * abstract lookup or replace the bilateral endpoint hook. */
static inline enum ug_reason ug_path_decision(u64 caller, u64 peer) {
    return caller == peer ? UG_ALLOWED : UG_FOREIGN_PEER;
}
static inline int ug_result(int prior, enum ug_reason reason) {
    if (prior) return prior;
    if (reason == UG_ALLOWED) return 0;
    return reason == UG_INTERNAL ? -UG_EIO : -UG_EPERM;
}
/* The actual socket namespace is authority; kern=1 is no owned-netns exemption. */
static inline enum ug_reason ug_creation_decision(u64 task, u64 namespace_owner,
                                                 int kernel_socket) {
    if (!task && !namespace_owner) return UG_ALLOWED;
    if (kernel_socket) return UG_INTERNAL;
    if (!task) return UG_FOREIGN_CALLER;
    if (task != namespace_owner) return UG_FOREIGN_NAMESPACE;
    return UG_ALLOWED;
}
static inline int ug_probe_matches(const struct ug_probe *p, u64 incarnation,
                                  u64 sequence) {
    return p && incarnation && sequence && p->incarnation == incarnation &&
           p->sequence == sequence && p->phase == UG_PROBE_SUBMITTED;
}
static inline int ug_initial_membership_matches(const struct ug_task *task,
                                                const struct ug_initial_task *initial) {
    if (!task || task->descendant || !task->incarnation || !task->initial_registration)
        return 0;
    return initial && initial->incarnation == task->incarnation &&
           initial->phase == UG_INITIAL_LIVE;
}
#endif
