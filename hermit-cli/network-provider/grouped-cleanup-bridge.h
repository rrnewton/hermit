/* SPDX-License-Identifier: BSD-3-Clause */
#ifndef HERMIT_GROUPED_CLEANUP_BRIDGE_H
#define HERMIT_GROUPED_CLEANUP_BRIDGE_H
#include "grouped-io.h"

#define HERMIT_GROUPED_CLEANUP_ABI 1U
#define HERMIT_GROUPED_CLEANUP_RECEIPT_BYTES (4U*AP_GROUPED_CENSUS_BYTES)
#define HERMIT_GROUPED_CLEANUP_CONTEXT_BYTES (64U*1024U)
#define HERMIT_GROUPED_CLEANUP_PUBLIC __attribute__((visibility("default")))
struct hermit_grouped_cleanup;

/* These are actual returned C-operation results, not invented individual
 * syscall returns. attempted/returned distinguish a durable/retained attempt
 * from a real return. Each alias close below is an actual close(2) result. */
struct hermit_grouped_cleanup_call { unsigned attempted,returned; int raw,error; };
struct hermit_grouped_cleanup_close {
    int descriptor;
    struct hermit_grouped_cleanup_call call;
};
struct hermit_grouped_cleanup_release {
    uint64_t release_start,first_failure_origin,enclosing_cutoff;
    unsigned has_first_failure;
};
/* One recovery ACK plus at most17 removal intent/outcome pairs. user_call is
 * the real external callback result; return_to_io includes any later original
 * cutoff failure. A missing callback is never represented as a returned0. */
struct hermit_grouped_cleanup_callback {
    unsigned kind; /* 1=recovery ACK, 2=removal journal */
    struct ap_grouped_owner owner;
    struct ap_grouped_write write;
    size_t line_bytes;
    char line[AP_GROUPED_LINE_BYTES];
    struct hermit_grouped_cleanup_call user_call,return_to_io;
};
struct hermit_grouped_cleanup_status {
    unsigned abi,refused;
    int first_error;
    /* adopted/deleted describe actual maintained helper0; outer API refusal
     * (for example a later cutoff) remains independently recorded and sticky. */
    unsigned prepared,adopted,deleted,absence_attempts,absence_complete_mask;
    unsigned aliases_attempted,aliases_returned,allocation_ready;
    uint64_t incarnation,stage_cutoff,effective_cutoff;
    uint64_t local_failure_origin;
    unsigned local_failure_origin_valid;
    int local_failure_clock_validation_error;
    size_t receipt_buffers_allocated;
    struct hermit_grouped_cleanup_call local_failure_clock;
    struct hermit_grouped_cleanup_release original;
    unsigned original_retained;
    size_t supplied_steps,retained_steps;
    struct hermit_grouped_cleanup_call allocation,prepare,adopt,deletion,absence[2],aliases;
    /* Actual maintained I/O helper returns are separate from outer API returns:
     * a later cutoff/callback failure may refuse an otherwise returned helper. */
    struct hermit_grouped_cleanup_call io_initialize,io_adopt,io_delete,io_absence[2];
    struct hermit_grouped_cleanup_close closes[AP_GROUP_FDS];
    /* Descriptive retained slots only; these integers confer no authority. */
    int descriptors[AP_GROUP_FDS];
    unsigned io_buffer_present,io_proof_present;
    struct ap_grouped_owner owner;
    unsigned writes_count;
    unsigned callbacks_count;
};
struct hermit_grouped_cleanup_history {
    struct ap_grouped_owner owner;
    unsigned writes_count;
    struct ap_grouped_write writes[AP_GROUPED_SITE_COUNT*2];
    size_t retained_steps;
    struct ap_grouped_recovery_step steps[AP_GROUPED_SITE_COUNT];
    unsigned callbacks_count;
    struct hermit_grouped_cleanup_callback callbacks[1+AP_GROUPED_SITE_COUNT*2];
};
/* Separate retained slot per attempt. Borrowed bytes remain valid until free,
 * including after alias retirement. A known complete-read span is identified
 * separately from a failed helper with unknown partial-byte extent. No stale
 * buffer prefix is labelled as newly observed bytes. */
struct hermit_grouped_cleanup_absence {
    struct hermit_grouped_cleanup_call call;
    struct hermit_grouped_cleanup_call io_call;
    struct ap_grouped_absence_observation observation;
    unsigned definition_bytes_known,profile_bytes_known;
    size_t definition_bytes,profile_bytes;
    const unsigned char *definitions,*profile;
};

HERMIT_GROUPED_CLEANUP_PUBLIC unsigned hermit_grouped_cleanup_abi(void);
/* Retain *out immediately after context allocation, before any later buffer
 * allocation can fail. At most4MiB receipt buffers,2MiB+2 unchanged I/O buffers
 * and the bounded64KiB context are owned. Even failure can leave a nonnull owned
 * context. All bounded buffers must be ready BEFORE any source creation ACK.
 * The caller stores this pointer outside cancellable operations. */
HERMIT_GROUPED_CLEANUP_PUBLIC int hermit_grouped_cleanup_alloc(struct hermit_grouped_cleanup **out);
/* One preparation attempt, before creation: store the original stage bound,
 * nonce/incarnation and callback; duplicate authenticated original controls
 * through unchanged ap_grouped_io_init. Partial buffers/FDs remain owned.
 * This API grants no native, provider, terminal, peer or cursor permission. */
HERMIT_GROUPED_CLEANUP_PUBLIC int hermit_grouped_cleanup_prepare(struct hermit_grouped_cleanup *,
    uint64_t incarnation,const char *nonce,size_t nonce_bytes,const int controls[3],
    uint64_t original_stage_cutoff,ap_grouped_journal,void *retained_owner);
/* UNSAFE CALLER PRECONDITIONS, enforced later by the actual isolated Rust
 * controller's private retained owner, never by serialized fields here:
 * - actual terminal source/helper/query/launcher custody, continuously owned
 *   original control OFDs and immutable independently retained source histories;
 * - a private NoProviderCreated value from the closed creation-only entry, or
 *   the separately proven full-provider terminal path (not an ap_open failure);
 * - a genuinely live authenticated independent cleanup peer and separate new
 *   durable cleanup journals; an archived source Keeper cannot acknowledge;
 * - exclusive cross-process CONTROL/PROFILE cursor ownership for every C call,
 *   yielding only at its real synchronous callback pause and regaining the
 *   same authenticated epoch BEFORE returning ACK0;
 * - immutable authenticated release/first-failure/enclosing origins, one-use
 *   ownership spanning all allocations, and original global/actor/FD gates.
 * The callbacks must not unwind or reenter. They succeed only after actual
 * current durable peer ACKs. Recovery uses only dual-intent-eligible source
 * steps; no original refused journal/bridge is reset. The operation retains
 * its attempt, original inputs and raw result; no retry or replacement context
 * can supply authority after uncertainty. Numeric C state is descriptive. */
HERMIT_GROUPED_CLEANUP_PUBLIC int hermit_grouped_cleanup_adopt(struct hermit_grouped_cleanup *,
    const struct ap_grouped_recovery_step *,size_t,ap_grouped_recovery_ack,void *,
    const struct hermit_grouped_cleanup_release *);
HERMIT_GROUPED_CLEANUP_PUBLIC int hermit_grouped_cleanup_delete(struct hermit_grouped_cleanup *);
/* Exactly two sequential attempts in distinct preallocated slots. A failed
 * first/second scan latches refusal; no retry, overwrite or later repair. */
HERMIT_GROUPED_CLEANUP_PUBLIC int hermit_grouped_cleanup_observe_absent(struct hermit_grouped_cleanup *);
/* Readback is available after failure and alias retirement. It is not an ACK
 * or capability and cannot be called during an active callback/native entry. */
HERMIT_GROUPED_CLEANUP_PUBLIC int hermit_grouped_cleanup_status(const struct hermit_grouped_cleanup *,struct hermit_grouped_cleanup_status *);
HERMIT_GROUPED_CLEANUP_PUBLIC int hermit_grouped_cleanup_history(const struct hermit_grouped_cleanup *,struct hermit_grouped_cleanup_history *);
HERMIT_GROUPED_CLEANUP_PUBLIC int hermit_grouped_cleanup_absence(const struct hermit_grouped_cleanup *,unsigned,struct hermit_grouped_cleanup_absence *);
/* One-shot LOCAL aliases only, even after failure. Record each real close
 * after occupying its slot; never retry a possibly reused descriptor. No
 * deletion/absence success follows from closing aliases. The caller retains
 * its independent global custody, histories and original primary failure. */
HERMIT_GROUPED_CLEANUP_PUBLIC int hermit_grouped_cleanup_release_aliases(struct hermit_grouped_cleanup *);
/* Caller must already have copied required readback and retain any unresolved
 * external/global custody. Requires completed explicit alias retirement.
 * Occupies *owned=NULL before free; never creates successful global cleanup. */
HERMIT_GROUPED_CLEANUP_PUBLIC int hermit_grouped_cleanup_free(struct hermit_grouped_cleanup **owned);
#endif
