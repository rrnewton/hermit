/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#ifndef HERMIT_GROUPED_BROKER_BRIDGE_H
#define HERMIT_GROUPED_BROKER_BRIDGE_H
#include <stddef.h>
#include <stdint.h>

#define HERMIT_GROUPED_BROKER_ABI 1U
#define HERMIT_GROUPED_BRIDGE_PUBLIC __attribute__((visibility("default")))
struct hermit_grouped_broker;
/* The owning Rust executable supplies its fixed sha2 implementation. This
 * separately packaged bridge has no ambient OpenSSL dependency. The callback
 * must return0 after writing exactly32 bytes; it must not unwind or reenter. */
typedef int (*hermit_grouped_digest)(const unsigned char *,size_t,unsigned char [32]);
struct hermit_grouped_broker_status {
    uint32_t abi,attempted,refused,error;
    uint32_t source_ready,source_created,successor_adopted,leaves_bound;
    uint32_t aliases_released,owner_phase,attempted_sites,verified_sites;
    uint64_t incarnation,deadline,creator_cutoff;
};
HERMIT_GROUPED_BRIDGE_PUBLIC unsigned hermit_grouped_broker_abi(void);
/* Allocate before fallible I/O. On successful allocation, *out is retained by
 * Rust recovery state before it invokes another entry. No numeric constructor
 * issues custody. Each operation below calls maintained real owner/I/O/wire
 * functions; failed operations retain the allocation and all partial FDs. */
HERMIT_GROUPED_BRIDGE_PUBLIC int hermit_grouped_broker_alloc(struct hermit_grouped_broker **);
/* Keeper creator authentication/ACK has already occurred through the actual
 * source endpoint. The original BEFORE-first-receipt cutoff is passed intact.
 * This entry receives the real independent guardian endpoint, authenticates
 * the same actual creator to it, then transfers controls to both holders. */
HERMIT_GROUPED_BRIDGE_PUBLIC int hermit_grouped_broker_source(struct hermit_grouped_broker *,int,
    uint64_t,const char *,uint64_t,uint64_t,const char *,const int [3]);
HERMIT_GROUPED_BRIDGE_PUBLIC int hermit_grouped_broker_create(struct hermit_grouped_broker *);
/* The keeper's one-use adoption ACK must prove genuine S1 terminal custody.
 * No serialized SourceTerminal object enters this ABI. Original control OFDs
 * arrive by actual SCM and are echoed with the maintained KCMP protocol. */
HERMIT_GROUPED_BRIDGE_PUBLIC int hermit_grouped_broker_successor(struct hermit_grouped_broker *,int,
    uint64_t,const char *,uint64_t,uint64_t,const char *,const int [3],hermit_grouped_digest);
/* Borrow only the actual retained adopted context. This does not create a
 * runtime cleanup owner or make the provider Ready: the private Rust caller
 * must separately join startup owners and retain the live runtime cleanup
 * handoff before admitting any guest operation. Neither pointer may outlive
 * this allocation or be reconstructed from status/JSON. */
struct ap_grouped_io;
struct ap_grouped_owner;
struct ap_grouped_write;
typedef int (*hermit_grouped_runtime_journal)(void *,const struct ap_grouped_owner *,
    const struct ap_grouped_write *,const char *);
/* Fixed raw local-close history; rows without attempted/returned bits are not
 * fabricated close results. This is descriptive evidence, never authority. */
struct hermit_grouped_runtime_handoff {
    uint32_t installed,aliases_released,attempted,returned;
    int32_t fd[9],raw[9],error[9];
};
/* PRIVATE UNSAFE CALLER CONTRACT: an actual retained runtime cleanup owner has
 * already acquired/ACKed original dual histories, continuous controls and the
 * exclusive cursor, before this one-use transfer. Its stable callback context
 * outlives all provider/cleanup calls. It joins original startup actors before
 * Ready. No raw callback, pointer or this return alone proves those predicates. */
/* Descriptive borrowed slots for actual SCM/KCMP comparison before handoff.
 * No duplication, lease issuance, callback/cursor transfer or Ready authority.
 * The enclosing retained Bridge must stay exclusively borrowed throughout. */
HERMIT_GROUPED_BRIDGE_PUBLIC int hermit_grouped_broker_runtime_controls(
    struct hermit_grouped_broker *,int [3]);
HERMIT_GROUPED_BRIDGE_PUBLIC int hermit_grouped_broker_runtime_journal(
    struct hermit_grouped_broker *,hermit_grouped_runtime_journal,void *);
HERMIT_GROUPED_BRIDGE_PUBLIC int hermit_grouped_broker_runtime_status(
    const struct hermit_grouped_broker *,struct hermit_grouped_runtime_handoff *);
HERMIT_GROUPED_BRIDGE_PUBLIC int hermit_grouped_broker_provider_lease(
    struct hermit_grouped_broker *,uint64_t,struct ap_grouped_io **,struct ap_grouped_owner **);
/* Only the private retained terminal owner may request this. No original
 * provider pointer lease was issued; C permanently refuses this route after
 * issuance. Actual original controller exit, live runtime peer and exclusive
 * cursor remain required, with the original release and earliest cutoff. */
HERMIT_GROUPED_BRIDGE_PUBLIC int hermit_grouped_broker_retire_unopened(
    struct hermit_grouped_broker *,uint64_t,uint64_t);
HERMIT_GROUPED_BRIDGE_PUBLIC int hermit_grouped_broker_status(const struct hermit_grouped_broker *,struct hermit_grouped_broker_status *);
/* Close only this helper's aliases. Never deletes definitions, signals a
 * process, restarts a deadline or asserts global absence. A raw close error
 * remains refused; it is not retried. External guardian/keeper custody remains. */
HERMIT_GROUPED_BRIDGE_PUBLIC int hermit_grouped_broker_release_aliases(struct hermit_grouped_broker *);
HERMIT_GROUPED_BRIDGE_PUBLIC int hermit_grouped_broker_free(struct hermit_grouped_broker *);
#endif
