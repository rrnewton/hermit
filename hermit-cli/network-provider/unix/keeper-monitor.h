/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause
 */
#ifndef HERMIT_UNIX_KEEPER_MONITOR_H
#define HERMIT_UNIX_KEEPER_MONITOR_H
#include "unix-guard.h"
/* The enclosing outside-parent/controller recovery owner retains all descriptors
 * and pinned links across every return from these functions. This module never
 * closes, detaches, unpins, signals a numeric PID, or declares terminal cleanup.
 * It is the concrete independent notification/status pump, not a session loader. */
struct ug_monitor_fds {
    int config, status, ring, other_actor_pidfd;
    u64 incarnation;
};
enum ug_terminal_class { UG_RUNNING = 0, UG_POLICY_REFUSAL, UG_INTERNAL_FAILURE };
enum ug_monitor_failure {
    UG_MONITOR_LOOKUP = 1, UG_MONITOR_CONFIG = 2, UG_MONITOR_STATUS = 4,
    UG_MONITOR_PEER_DEAD = 8, UG_MONITOR_POLL = 16,
};
struct ug_monitor_result {
    enum ug_terminal_class primary;
    u64 secondary_monitor_failures, secondary_guard_faults;
    struct ug_denial denial;
};
void ug_monitor_note_failure(struct ug_monitor_result *, u64);
/* Pure decode, also used by the actual map-read path. */
int ug_monitor_decode(const struct ug_config *, const struct ug_status *, u64,
                      struct ug_monitor_result *);
/* One bounded wait/status observation. Both actors run this pump independently
 * of guest scheduler/RPC progress. poll has a 50ms maximum wait even when ring
 * notification fails; a ready other-actor pidfd is an internal terminal event.
 * A prior result is preserved, including a typed policy primary followed by
 * ring failure, peer death or another teardown failure. */
/* Locked typed snapshot without treating an expected terminal helper as failure. */
int ug_monitor_snapshot(const struct ug_monitor_fds *, struct ug_monitor_result *);
int ug_monitor_once(const struct ug_monitor_fds *, struct ug_monitor_result *);
#endif
