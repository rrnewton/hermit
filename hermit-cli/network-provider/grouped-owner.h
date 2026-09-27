/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#ifndef HERMIT_GROUPED_OWNER_H
#define HERMIT_GROUPED_OWNER_H
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>
#include "grouped-probes.h"
#define AP_GROUPED_NONCE_BYTES 32U
#define AP_GROUPED_NAME_BYTES 64U
#define AP_GROUPED_LINE_BYTES 256U
#define AP_GROUPED_CENSUS_BYTES (1U<<20)
enum ap_grouped_phase {
    AP_GROUPED_EMPTY,AP_GROUPED_ACKED,AP_GROUPED_CREATING,
    AP_GROUPED_CREATED,AP_GROUPED_LEAVES,AP_GROUPED_ACTIVE,
    AP_GROUPED_QUIESCENT,AP_GROUPED_CLEANING,AP_GROUPED_ABSENT,
    AP_GROUPED_UNKNOWN
};
struct ap_grouped_owner {
    uint64_t incarnation;
    char group[AP_GROUPED_NAME_BYTES],event[AP_GROUPED_NAME_BYTES];
    enum ap_grouped_phase phase;
    uint32_t verified_sites,attempted_sites,event_id;
    unsigned write_unknown,pending_role,pending_remove;
    size_t pending_bytes;
};
/* The surrounding existing private owner authenticates and retains actual
 * pidfd/unit/cgroup/nonce reservation and protected control descriptions.
 * ACK may only be called after that real receipt, before the first write.
 * This pure state does not invent or replace that external authority. */
int ap_grouped_owner_init(struct ap_grouped_owner *,uint64_t,const char *,size_t);
int ap_grouped_owner_ack(struct ap_grouped_owner *,uint64_t,const char *,size_t);
int ap_grouped_census(const struct ap_grouped_owner *,const void *,size_t,uint32_t *);
int ap_grouped_profile(const struct ap_grouped_owner *,const void *,size_t,unsigned);
int ap_grouped_command(const struct ap_grouped_owner *,unsigned,int,char *,size_t);
/* A write is journaled before invocation; success alone never proves effect.
 * Caller makes ONE exact write, retains raw return/errno, then supplies a
 * complete fresh census. Unknown write is sticky and bars activation. */
int ap_grouped_create_begin(struct ap_grouped_owner *,const void *,size_t,unsigned,char *,size_t);
int ap_grouped_create_observed(struct ap_grouped_owner *,unsigned,ssize_t,size_t,const void *,size_t);
int ap_grouped_bind_leaves(struct ap_grouped_owner *,const void *,size_t,const void *,size_t,const void *,size_t);
int ap_grouped_activate(struct ap_grouped_owner *,const void *,size_t,const void *,size_t);
/* Called only after actual guest/controller terminal custody and complete
 * Call ACK/retirement. A zero thread count alone is not that authority. */
int ap_grouped_quiescent(struct ap_grouped_owner *);
/* After exact provider terminal custody, the retained owner may reconcile
 * failed/partial startup. A namespace collision before our first write never
 * authorizes deleting a matching pre-existing object. */
int ap_grouped_recover(struct ap_grouped_owner *,const void *,size_t);
int ap_grouped_delete_begin(struct ap_grouped_owner *,const void *,size_t,unsigned,char *,size_t);
int ap_grouped_delete_observed(struct ap_grouped_owner *,unsigned,ssize_t,size_t,const void *,size_t);
int ap_grouped_absent(struct ap_grouped_owner *,const void *,size_t,const void *,size_t,int);
#endif
