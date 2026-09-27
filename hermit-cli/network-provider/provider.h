#ifndef HERMIT_ACCEPTED_PROVIDER_H
#define HERMIT_ACCEPTED_PROVIDER_H
#ifndef __BPF__
#include <stdint.h>
typedef uint64_t u64;
typedef int64_t s64;
typedef uint32_t u32;
typedef int32_t s32;
typedef uint16_t u16;
typedef uint8_t u8;
#endif
/* Object/creation observations retain their fixed non-evicting capacities.
 * Command cells are bounded separately and require explicit exact retirement.
 * Exhaustion never silently reuses a still-owned identity. */
#define AP_OBJECTS 64
#define AP_EVENTS 32
#define AP_CALLS 64
#define AP_COMMANDS 32
/* Slot zero remains diagnostic-only. Tickets never wrap or identify a slot alone. */
#define AP_COMMAND_SLOTS (AP_COMMANDS - 1)
enum ap_command_phase { AP_COMMAND_DONE=1, AP_COMMAND_READY=2, AP_COMMAND_RUNNING=3 };
#ifdef __BPF__
static __attribute__((noinline)) u32
#else
static __attribute__((always_inline)) inline u32
#endif
ap_command_slot(u64 ticket) {
    return ticket ? 1 + (u32)((ticket - 1) % AP_COMMAND_SLOTS) : 0;
}
#define AP_AF_INET 2
#define AP_AF_INET6 10
#define AP_SOL_SOCKET 1
#define AP_SO_COOKIE 57
#define AP_TCP_SYN_RECV 3
#define AP_TCP_CLOSE 7
#define AP_TCP_LISTEN 10
#define AP_IPPROTO_TCP 6
struct ap_identity { u64 provider, object, namespace; };
/* Private map key shared by the BPF producer and retained-command collector. */
struct ap_invocation_key { u64 sock, request, task, start; };
struct ap_raw_state {
    s64 receive_timeout_ticks, send_timeout_ticks;
    s32 lowat, receive_buffer, peek_offset, socket_option_memory;
    u32 window_clamp;
    u8 userlocks, scaling_ratio, tcp_state, child_spin_locked;
};
struct ap_endpoint4 { u32 address_be; u16 port_be, family; };
struct ap_creation {
    u64 sequence;
    struct ap_identity listener, child;
    u64 listener_generation, mutation_epoch_enter, mutation_epoch_exit;
    u64 overlap; /* Sticky: this observation is NOT a semantic certificate. */
    struct ap_raw_state listener_before, listener_after, child_created;
    struct ap_endpoint4 local, peer;
    u64 cookie_at_creation;
    u64 phase; /* Published last, then only atomic lifecycle OR operations. */
};
enum ap_phase { AP_CREATED=1, AP_QUEUED=2, AP_RETIRED=4, AP_MATCHED=8 };
enum ap_operation { AP_ENROLL=1, AP_MATCH=2, AP_SETTER=3,
    /* 4..13 belong to the shared original-effect observer. This auxiliary
     * observation does not allocate a socket/file identity or execute a guest
     * syscall. An unknown/different held file is retained as an observation. */
    AP_OBSERVE_SOCKET_FILE=14,
};
struct ap_task_command {
    u64 provider, command, operation, expected_object;
    u64 generation_before, generation_after;
    s32 expected_level, expected_option;
    /* ABI7: exact original-I/O count operand; op11 uses bytes, older ops use zero. */
    u64 original_count;
};
static __attribute__((always_inline)) inline int ap_socket_file_observation_command(
    const struct ap_task_command *c) {
    return c && c->provider && c->command && c->operation==AP_OBSERVE_SOCKET_FILE &&
        !c->expected_object && !c->generation_before && !c->generation_after &&
        !c->expected_level && !c->expected_option && !c->original_count;
}
struct ap_command_result {
    u64 command, operation, task, start_boottime;
    struct ap_identity identity;
    u64 creation, cookie;
    struct ap_raw_state state;
    s32 returned;
    u32 reserved;
    u64 phase;
    /* ABI7 operand echo; set before the final phase publication. */
    u64 original_count;
};
/* This predicate is shared with the actual BPF producer claim. A ticket is
 * authority only when its bounded physical slot holds that exact reservation. */
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_command_reservation_matches(
    const struct ap_task_command *c,const struct ap_command_result *r,u32 slot) {
    return c && r && c->command && slot>0 && slot<AP_COMMANDS &&
        slot==ap_command_slot(c->command) && r->command==c->command &&
        r->operation==c->operation && r->original_count==c->original_count &&
        r->phase==AP_COMMAND_READY;
}
/* Canonical untouched READY payload only. Reservation identity/phase and
 * original operands remain separately mandatory at each caller. These exact
 * fields are stable in an unclaimed cell; this predicate publishes nothing. */
#ifdef __BPF__
static __attribute__((noinline)) int
#else
static __attribute__((always_inline)) inline int
#endif
ap_command_ready_payload_empty(const struct ap_command_result *r) {
    return r && !(r->task || r->start_boottime || r->identity.provider || r->identity.object ||
        r->identity.namespace || r->creation || r->cookie || r->returned || r->reserved ||
        r->state.receive_timeout_ticks || r->state.send_timeout_ticks || r->state.lowat ||
        r->state.receive_buffer || r->state.peek_offset || r->state.socket_option_memory ||
        r->state.window_clamp || r->state.userlocks || r->state.scaling_ratio ||
        r->state.tcp_state || r->state.child_spin_locked);
}
/* Diagnostic overlay of reserved commands[0]; normal commands are1..31.
 * Exact136B like ap_command_result in ABI7. Never an authorization/result receipt. */
struct ap_setter_rejection {
    u64 phase, mismatch, task, start_boottime, incarnation, object, generation;
    s32 level, option;
    struct ap_task_command raw;
    u32 raw_present;
    s32 raw_level; /* Unused ctx1 retained separately from semantic level. */
};
struct ap_config {
    u64 provider;
#ifdef AP_GROUPED_PROVIDER
    /* Private successor map ABI. The loader owns the actual bootstrap task
     * through its held pidfd and admits no guest before the one-use receipt. */
    u64 anchor_task,anchor_start,anchor_ip,anchor_phase;
    /* Values read from the two exact-image x86 direct-map globals during the
     * one-use bootstrap. Their storage addresses are anchor-relative; neither
     * variable is exported in this kernel's BTF. */
    u64 vmemmap_base,page_offset_base;
#define AP_GROUPED_ANCHOR_ARMED 1ULL
#define AP_GROUPED_ANCHORED 2ULL
#define AP_GROUPED_ANCHOR_ACTIVE 3ULL
#endif
};
struct ap_status {
    u64 fatal, next_object, next_creation, clone_entries, clone_null_returns;
    u64 created, queued, retired, matched, setters_entered, setters_exited;
};
enum ap_fatal {
    AP_CAPACITY=1, AP_DUPLICATE=2, AP_MISSING=4, AP_BAD_COMMAND=8,
    AP_WRONG_IDENTITY=16, AP_NOT_TCP4=32, AP_UNKNOWN_MUTATION=64,
    AP_MUTATION_OVERLAP=128, AP_CHILD_UNLOCKED=256, AP_MAP_FAILURE=512,
    AP_ENDPOINT=1024, AP_STALE_GENERATION=2048, AP_UNSUPPORTED_STATE=4096,
};
/* Non-BPF consumers must validate all payload/phase/counter constraints before
 * converting raw evidence to ChildCreationCertificate. These types do not
 * confer custody: callers retain the actual listener/accepted OwnedFd. */
#ifndef __BPF__
struct ap_session;
struct ap_program_id { u32 kind, id; };
int ap_open(const char *object_path, u64 incarnation, struct ap_session **out);
int ap_register_task(struct ap_session *, int exact_pidfd);
/* Exact original auxiliary owner only; requires collected command ACK first. */
int ap_retire_auxiliary_task(struct ap_session *, int exact_worker_pidfd);
int ap_enroll_listener(struct ap_session *, int exact_pidfd, int held_socket_fd,
                       u64 semantic_generation, struct ap_command_result *);
int ap_prepare_setter(struct ap_session *, int exact_pidfd,
                      struct ap_identity, u64 before, u64 after,
                      int level, int option, u64 *command);
int ap_finish_setter(struct ap_session *, int exact_pidfd, u64 command,
                     struct ap_command_result *);
int ap_resolve_accepted(struct ap_session *, int exact_pidfd, int held_socket_fd,
                        struct ap_command_result *);
int ap_match_accepted(struct ap_session *, int exact_pidfd, int held_socket_fd,
                      struct ap_identity expected, struct ap_command_result *);
/* The caller owns held_socket_fd throughout SO_COOKIE. identity.object is the
 * existing fd_files generation, or zero if the actual held file is unknown.
 * Success means a complete observation, not equality with the original file.
 * The original-installation consumer must check that equality itself. */
int ap_observe_socket_file(struct ap_session *, int exact_helper_pidfd,
                           int held_socket_fd, struct ap_command_result *);
/* Call only after the service has durably retained the complete validated
 * receipt. Exact comparison includes provider, ticket, task incarnation, and
 * every payload byte. Failure is never permission to retry a new operation.
 * A repeated/stale ACK returns ESTALE and cannot clear a replacement slot. */
int ap_ack_command(struct ap_session *, const struct ap_command_result *exact_receipt);
int ap_read_creation(struct ap_session *, u32 sequence, struct ap_creation *);
int ap_read_status(struct ap_session *, struct ap_status *);
int ap_read_setter_rejection(struct ap_session *, struct ap_setter_rejection *);
int ap_validate_creation(const struct ap_creation *, const struct ap_status *);
int ap_identifiers(struct ap_session *, struct ap_program_id *, u32 capacity, u32 *written);
int ap_close(struct ap_session *);
#endif
#endif
