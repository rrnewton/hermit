/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#ifndef HERMIT_UNIX_KEEPER_READBACK_H
#define HERMIT_UNIX_KEEPER_READBACK_H
#include <stdint.h>
#define UG_INVENTORY_MAGIC UINT64_C(0x5547494E56303031)
#define UG_INVENTORY_MAX 72
/* Plain identifiers only. This object contains no FD or kernel reference. */
struct ug_plain_id { uint32_t kind, id; }; /* 0 map, 1 program, 2 link */
struct ug_inventory {
    uint64_t magic, incarnation, proof_sequence, record_ordinal;
    uint32_t count, maps, programs, links;
    struct ug_plain_id ids[UG_INVENTORY_MAX];
};
/* Provisional object-close API completion, never an absence certificate. */
struct ug_object_close {
    uint64_t incarnation, proof_sequence, record_ordinal, closed_ns;
    uint64_t deadline_ns, count, first_outcome, guard_faults;
};
struct ug_readback_receipt {
    struct ug_inventory inventory;
    struct ug_object_close closed;
    uint64_t observed_ns, complete_passes;
};
/* Caller owns an immutable sealed inventory FD; this entry only issues
 * GET_FD_BY_ID and closes each temporary query description. No load/attach,
 * policy mutation, pin removal or socket operation exists in this component. */
int ug_read_inventory(int inventory_fd, struct ug_inventory *);
int ug_readback_inventory(int inventory_fd, const struct ug_object_close *,
                          struct ug_readback_receipt *);
/* Shared metadata query only. Recovery callers cannot mint original-close
 * certificates and must retain their distinct later observation interval. */
int ug_query_absence(const struct ug_plain_id *, uint32_t count,
                     uint64_t begin, uint64_t deadline,
                     uint64_t *observed, uint64_t *passes);
#endif
