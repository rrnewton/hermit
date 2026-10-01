/* SPDX-License-Identifier: MIT */
#ifndef HERMIT_OWNED_METADATA_H
#define HERMIT_OWNED_METADATA_H
#include "provider.h"
#include <linux/bpf.h>
#include "stream-copy-fault.h"
/* Copied metadata, never an FD or object-lookup capability. Link-backed
 * program evidence is deliberately NOT a bpf_prog_info snapshot. */
enum ap_owned_metadata_source { AP_OWNED_DIRECT_FD=1, AP_OWNED_PROGRAM_LINK=2 };
struct ap_owned_metadata {
    struct ap_program_id identity;
    u32 source,version;
    union {
        struct bpf_map_info map;
        struct bpf_prog_info program;
        struct bpf_link_info link;
        struct { u32 program_type,reserved;struct bpf_link_info link; } linked_program;
    } value;
};
int ap_owned_object_info(struct ap_session *,struct ap_program_id,struct ap_owned_metadata *);
/* Read-only diagnostic snapshot for the exact still-owned original Read.
 * This does not collect/ACK a command or certify a failed partial stream. */
int ap_original_read_fault_snapshot(struct ap_session *,u64 command,u32 map_id,
                                    struct ap_stream_fault_state *);
#endif
