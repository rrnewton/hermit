/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause
 */
#ifndef HERMIT_UNIX_KEEPER_SESSION_H
#define HERMIT_UNIX_KEEPER_SESSION_H
#include "keeper-monitor.h"
#include "keeper-readback.h"
#define UG_LINKS 31
#define UG_MAPS 10
/* Exact zero-error RECORD_FAILURE payload for a clean keeper-local policy
 * observation. Generic errors, including ECANCELED, never carry this marker. */
#define UG_JOURNAL_POLICY_OBSERVED "keeper-policy-v1"
struct ug_session;
/* Implemented by the exec-created outside keeper only. Inputs are borrowed
 * capabilities: a held immutable ELF, bpffs directory and regular recovery
 * directory. The returned owner exists even on partial failure. No destructor
 * or error path in this interface unpins/detaches an admitted policy. */
int ug_session_open(int elf, int bpffs_root, int recovery_root, u64 incarnation,
                    struct ug_session **out);
/* Obtain new CLOEXEC, read-only map descriptions through the exact pins.
 * All three results remain owned by the caller, including a partial error. */
int ug_session_readers(struct ug_session *, int out[3]);
/* Writable birth-map capability for outside-only synchronous recovery. Obtain
 * before ARM; the child must close its inherited copy before backend startup.
 * This does not authorize a new creator or supply namespace identity. */
int ug_session_creator_recovery(struct ug_session *, int *out);
/* Caller supplies a held pidfd received over the single private control
 * channel. The outside caller must be the exact thread about to call clone;
 * arming does not enroll it as a guest. One creator command per session. */
int ug_session_arm_creator(struct ug_session *, int held_pidfd, u64 sequence);
int ug_session_observe_birth(struct ug_session *, u64 sequence, struct ug_birth *);
/* An externally authenticated stopped initial guest, not the tracer. Partial
 * insertion retains its held pidfd + STAGED/LIVE row and cannot authorize run. */
int ug_session_register_initial(struct ug_session *, int held_pidfd, u64 sequence);
/* Session-owned observation history also serves TERMINAL. Keep pumping after
 * policy: a later independent failure must remain in the original journal. */
int ug_session_monitor(struct ug_session *, int other_actor_pidfd, u64 sequence);
/* Record actor failure durably while retaining every map/link/pidfd. This is
 * not terminal proof. Process exit leaves the completed bpffs pins in place. */
int ug_session_note_failure(struct ug_session *, u64 sequence, int error);
/* Parent-only control lane binds the actual Container task before READY.
 * A terminal request closes further admissions before examining any lifetime.
 * EAGAIN retains all pins/owners and means a known lifetime is still active;
 * any other failed proof also retains pins. A refusal is not a cleanup proof. */
int ug_session_bind_controller(struct ug_session *, int held_pidfd, u64 sequence);
struct ug_terminal_receipt {
    u64 incarnation, sequence, record_ordinal, removed_links;
    u64 removed_map_pins, initial_tasks, first_outcome, guard_faults;
};
/* Legacy lower-level detach API retained for the unchanged no-ARM controls.
 * It is not a production aggregate certificate and is not dispatched by keeper. */
int ug_session_terminal(struct ug_session *, u64 sequence, struct ug_terminal_receipt *);
/* Production phase1: terminal lifetimes, exact immutable original inventory,
 * detach/unpin; object FD descriptions remain until explicit parent ACK. */
int ug_session_prepare_terminal(struct ug_session *, u64 sequence,
                                struct ug_terminal_receipt *, int *inventory_fd);
/* Production phase2: exact phase1 ACK after parent/controller readers close.
 * The result timestamps final object-close API completion and fixes the single
 * one-second ID readback deadline. It does NOT claim those IDs are absent. */
int ug_session_close_terminal(struct ug_session *, u64 sequence,
                              u64 proof_sequence, u64 proof_ordinal,
                              u64 original_terminal_deadline_ns,
                              struct ug_object_close *);
/* Success proves links absent and owned pin directory removed. Read-only map
 * descriptions held by the parent/controller remain their explicit owners;
 * their later close is required before aggregate map-ID absence. */
#endif
