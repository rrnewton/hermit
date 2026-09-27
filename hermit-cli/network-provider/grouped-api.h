/* SPDX-License-Identifier: BSD-3-Clause */
#ifndef HERMIT_GROUPED_API_H
#define HERMIT_GROUPED_API_H
#include "provider.h"
#include "grouped-io.h"
/* Explicit successor API: legacy ap_open in this DSO refuses ENOTSUP. The
 * existing private bootstrap owns the actual acknowledged, nondumpable
 * cleanup holder, nonce reservation and exact manager OpenFile roles. It
 * retains io/owner through all failures until authoritative global absence.
 * All startup broker actors must be terminal before admitting guest work.
 * No guest inherits these descriptions or has a permitted /proc reopen. */
int ap_open_grouped(const char *,u64,struct ap_grouped_io *,struct ap_grouped_owner *,u64,struct ap_session **);
/* Caller must already hold actual guest/controller terminal custody and have
 * completed every Call ACK/retirement. The supplied timestamp is the ORIGINAL
 * before-all-releases timestamp; this API cannot establish a later origin.
 * The separate retained owner still proves all actual BPF IDs absent twice,
 * global event/definitions absent, FD population restored and actors terminal
 * inside that same original1s. Any failed close/delete remains failed even
 * if later operational recovery proves absence. */
int ap_close_grouped_terminal(struct ap_session **,u64);
/* Additive tighter-cutoff entry; the legacy ACTIVE body remains unchanged. */
int ap_close_grouped_terminal_until(struct ap_session **,u64,u64);
/* Failed open only, never an alternate ACTIVE close. The retained caller has
 * the original controller terminal proof, exact original LEAVES lease, live
 * cleanup peer and exclusive cursor; NULL *owned records a real no-session
 * result. Any acquired session must still be unready. Native partial BPF
 * owners close before the existing full-history reconcile/reverse deletion.
 * Raw NULL does not mean that the broker's 17 definitions are absent. */
int ap_close_grouped_startup_terminal(struct ap_session **,struct ap_grouped_io *,
    struct ap_grouped_owner *,u64,u64);

#endif
