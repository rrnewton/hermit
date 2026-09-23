/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause
 */
#define _GNU_SOURCE
#include "keeper-monitor.h"
#include <errno.h>
#include <linux/bpf.h>
#include <poll.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

void ug_monitor_note_failure(struct ug_monitor_result *r, u64 failure) {
    if (!r->primary) r->primary = UG_INTERNAL_FAILURE;
    r->secondary_monitor_failures |= failure;
}
int ug_monitor_decode(const struct ug_config *c, const struct ug_status *s,
                      u64 incarnation, struct ug_monitor_result *r) {
    if (!c || !s || !incarnation || c->incarnation != incarnation ||
        c->policy != UG_DENY || c->abi_version != UG_ABI_VERSION) {
        ug_monitor_note_failure(r, UG_MONITOR_CONFIG); return -1;
    }
    r->secondary_guard_faults |= s->faults;
    switch (s->first_outcome) {
    case 0:
        /* During publication the BPF task may have written a fault or denial
         * payload but not first_outcome yet. Do not turn an incomplete snapshot
         * into either a policy verdict or a clean completion certificate. */
        if (s->faults || s->first_denial.phase) return 1;
        return 0;
    case UG_EVENT_INTERNAL:
        if (!r->primary) r->primary = UG_INTERNAL_FAILURE;
        return 0;
    case UG_EVENT_DENIAL:
        if (s->first_denial.phase != 2 ||
            s->first_denial.incarnation != incarnation ||
            (s->first_denial.reason != UG_FOREIGN_SOCKET &&
             s->first_denial.reason != UG_FOREIGN_CALLER &&
             s->first_denial.reason != UG_FOREIGN_PEER &&
             s->first_denial.reason != UG_FOREIGN_NAMESPACE)) {
            ug_monitor_note_failure(r, UG_MONITOR_STATUS); return -1;
        }
        if (!r->primary) {
            r->primary = UG_POLICY_REFUSAL;
            r->denial = s->first_denial;
        }
        return 0;
    default:
        ug_monitor_note_failure(r, UG_MONITOR_STATUS); return -1;
    }
}
static int map_value(int fd, void *value, u64 flags) {
    u32 key = 0;
    union bpf_attr a = {0};
    a.map_fd = fd;
    a.flags = flags;
    a.key = (u64)(uintptr_t)&key;
    a.value = (u64)(uintptr_t)value;
    return (int)syscall(SYS_bpf, BPF_MAP_LOOKUP_ELEM, &a, sizeof(a));
}
int ug_monitor_snapshot(const struct ug_monitor_fds *f, struct ug_monitor_result *r) {
    if(!f || !r || f->config<0 || f->status<0 || !f->incarnation) {errno=EINVAL;return -1;}
    struct ug_config c;
    struct ug_status s;
    if (map_value(f->config, &c, 0) || map_value(f->status, &s, BPF_F_LOCK)) {
        ug_monitor_note_failure(r, UG_MONITOR_LOOKUP); return -1;
    }
    return ug_monitor_decode(&c, &s, f->incarnation, r);
}
int ug_monitor_once(const struct ug_monitor_fds *f, struct ug_monitor_result *r) {
    if (!f || !r) { errno = EINVAL; return -1; }
    if (f->config < 0 || f->status < 0 || f->ring < 0 ||
        f->other_actor_pidfd < 0 || !f->incarnation) {
        ug_monitor_note_failure(r, UG_MONITOR_CONFIG); errno = EINVAL; return -1;
    }
    /* First inspect status so a committed primary survives a later actor death.
     * BPF first_outcome orders competing first failures; errno/status numbers do
     * not determine policy classification. */
    int before = ug_monitor_snapshot(f, r);
    if (before < 0 || r->primary) return before;
    struct pollfd fds[] = {
        { .fd = f->ring, .events = POLLIN },
        { .fd = f->other_actor_pidfd, .events = POLLIN },
    };
    int result = poll(fds, 2, 50);
    int saved_errno = errno;
    /* Read even on timeout, EINTR, poll error, or actor death. A failed ring
     * write cannot leave an all-blocked guest waiting for a later syscall. */
    int after = ug_monitor_snapshot(f, r);
    if (result < 0 && saved_errno != EINTR) ug_monitor_note_failure(r, UG_MONITOR_POLL);
    if (result > 0 && (fds[0].revents & (POLLERR | POLLNVAL | POLLHUP)))
        ug_monitor_note_failure(r, UG_MONITOR_POLL);
    if (result > 0 && (fds[1].revents & (POLLERR | POLLNVAL)))
        ug_monitor_note_failure(r, UG_MONITOR_POLL);
    if (result > 0 && (fds[1].revents & (POLLIN | POLLHUP)))
        ug_monitor_note_failure(r, UG_MONITOR_PEER_DEAD);
    /* A publication-in-progress response requires another pump iteration and
     * cannot authorize guest copyout, even if no primary exists yet. */
    return after;
}
