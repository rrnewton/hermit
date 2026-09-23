/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause
 */
#ifndef HERMIT_UNIX_KEEPER_CHANNEL_H
#define HERMIT_UNIX_KEEPER_CHANNEL_H
#include "keeper-session.h"
#define UG_WIRE_MAGIC 0x55474B4545503031ULL
#define UG_WIRE_RIGHTS 4
enum ug_operation { UG_INIT=1, UG_ARM, UG_BIRTH, UG_INITIAL, UG_STOP, UG_CREATOR_RECOVERY, UG_CONTROLLER_CHANNEL, UG_CONTROLLER_TASK, UG_TERMINAL,
                    UG_RESPONSE=0x100, UG_OUTCOME=0x200 };
struct ug_frame {
    u64 magic, incarnation, sequence;
    u32 operation, rights;
    s32 error; u32 reserved;
    u64 values[8];
};
struct ug_packet { struct ug_frame frame; int fds[UG_WIRE_RIGHTS]; u32 count; };
/* No raw descriptor is encoded in a frame. A successful send duplicates only
 * the supplied held capabilities over SCM_RIGHTS; caller retains originals.
 * Every receive result (including malformed/truncated input) retains any actual
 * received descriptors in packet until its owner settles that error. */
int ug_channel_send(int channel,const struct ug_packet *);
int ug_channel_receive(int channel,struct ug_packet *);
/* Finite host-monotonic request. Output always owns all received descriptors,
 * including timeout/protocol error. No retries of submitted commands. */
int ug_channel_request(int channel,const struct ug_packet *,struct ug_packet *,
                       u64 monotonic_deadline_ns);
/* verified_response becomes1 ONLY after complete, nontruncated receive and
 * exact run/sequence/operation validation; a remote errno remains an error.
 * A plausible frame after a malformed receive is not a verified response. */
int ug_channel_request_observed(int channel,const struct ug_packet *,struct ug_packet *,
                                u64 monotonic_deadline_ns,int *verified_response);
/* Synchronous outside-creator recovery. Caller is the exact alive creator,
 * on the same thread with every catchable signal blocked, after clone returned
 * (or before clone was attempted). No other thread/handler can use this arm.
 * ack=0 requires actual helper terminal before ANY birth-map lookup: timeout or
 * wrapper exit cannot prove that a queued ARM will not still insert a command.
 * Success returns an inert consumed command or exact unused removal+absence.
 * Failure never authorizes signal-mask restore; the caller retains recovery.
 * No wait, policy detach, or socket/reference release occurs here. */
/* Kernel x86-64 rt_sigprocmask representation, not glibc's larger sigset_t.
 * Raw syscall also blocks libc-reserved catchable signals during the exact
 * single-threaded creator window. No implicit restore on owner destruction. */
struct ug_creator_mask { u64 original; s32 tid; u32 active; };
int ug_creator_mask_block(struct ug_creator_mask *);
int ug_creator_mask_restore(struct ug_creator_mask *, int child_branch);
enum ug_creator_terminal { UG_CREATOR_REMOVED=1, UG_CREATOR_CONSUMED, UG_CREATOR_ABSENT };
int ug_creator_terminalize(int birth_map, int creator_pidfd, int helper_pidfd,
                           u64 incarnation, u64 sequence, int acknowledged,
                           struct ug_birth *observed);
/* Entry only in an exec-created outside helper, stdin is the owned private
 * SEQPACKET endpoint. Never invoked inside the guest namespace/controller. */
int ug_keeper_main(void);
#endif
