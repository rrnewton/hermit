/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#ifndef HERMIT_GROUPED_IO_H
#define HERMIT_GROUPED_IO_H
#include "grouped-owner.h"
#include <sys/stat.h>
enum ap_grouped_fd_role {AP_GROUP_CONTROL,AP_GROUP_PROFILE,AP_GROUP_EVENTS,
    AP_GROUP_ID,AP_GROUP_FORMAT,AP_GROUP_ENABLE,AP_GROUP_FDS};
struct ap_grouped_write {
    unsigned role,remove;
    size_t submitted;
    ssize_t raw;
    int error,started,completed;
};
/* Immutable exact before/after callback bytes held by the authenticated keeper.
 * Both owner snapshots are the pending-create state: the existing callback
 * runs before ap_grouped_create_observed reads the resulting census. */
struct ap_grouped_creation_pair {
    struct ap_grouped_owner intent_owner,outcome_owner;
    struct ap_grouped_write intent,outcome;
    char line[AP_GROUPED_LINE_BYTES];
};
/* This callback must complete the actual keeper protocol: same S2 creator
 * pidfd/cgroup/peer credentials; S1 fully terminal; immutable exact17 history;
 * returned SCM duplicates self-KCMP_FILE-equal to the continuously retained
 * three controls; fresh full census; one-use transfer consumed before ACK.
 * Merely comparing numbers, inodes, or an application boolean is insufficient.
 * Keeper custody remains live through any missing/ambiguous ACK and afterward
 * until final original-deadline release proof. The caller retains raw history
 * and transport custody even if this API returns failure. */
typedef int (*ap_grouped_adoption_ack)(void *,const struct ap_grouped_owner *,
    const int [3],const struct ap_grouped_creation_pair *,size_t);
typedef int (*ap_grouped_journal)(void *,const struct ap_grouped_owner *,const struct ap_grouped_write *,const char *);
struct ap_grouped_io {
    int fd[AP_GROUP_FDS];
    struct stat identity[AP_GROUP_FDS];
    char *buffer,*proof;
    struct ap_grouped_write writes[AP_GROUPED_SITE_COUNT*2];
    unsigned writes_count;
    ap_grouped_journal journal;
    void *journal_context;
};
/* The existing authenticated manager/private bootstrap must supply these
 * exact roles. No arbitrary path is accepted here. The caller retains its
 * source descriptions; each successful duplicate below becomes io custody,
 * including on failure. A partial init/join must always be released. */
/* journal must obtain the retained cleanup owner's exact durable intent ACK
 * before returning0. A missing callback cannot create or delete global state.
 * It also retains each actual write outcome; failure keeps UNKNOWN custody. */
int ap_grouped_io_init(struct ap_grouped_io *,const int [3],ap_grouped_journal,void *);
int ap_grouped_io_join(struct ap_grouped_io *,const int [3]);
int ap_grouped_io_release(struct ap_grouped_io *);
/* These are real tracefs effects, never called by pure controls. The existing
 * owner supplies absolute startup deadline and exact acknowledged state. */
int ap_grouped_io_create(struct ap_grouped_io *,struct ap_grouped_owner *,uint64_t);
/* Read-only adoption, never another create. Only fresh io and EMPTY owner;
 * reconstruct the exact FSM privately, obtain real keeper authority, then
 * require a fresh full tuple/profile census before exposing CREATED. Existing
 * io_join/io_bind remain mandatory. No raw struct counter grants ownership. */
int ap_grouped_io_adopt_created(struct ap_grouped_io *,struct ap_grouped_owner *,
    const struct ap_grouped_creation_pair *,size_t,ap_grouped_adoption_ack,void *,uint64_t);
/* Recovery-only history: successful ordered pairs plus at most a final
 * incomplete/failed write. Absence is explicit, never synthesized as raw0.
 * With no outcome, outcome_owner and outcome must be entirely zero. */
struct ap_grouped_recovery_step {
    struct ap_grouped_creation_pair pair;
    unsigned outcome_present;
};
/* The actual retained keeper/receiver must authenticate immutable exact steps,
 * continuously held original controls by SCM/KCMP or its own retained custody,
 * exact incarnation/nonce, terminal creator and no active provider, and consume
 * its one-use RECOVERY offer. No numeric owner/FD snapshot or elapsed time can
 * supply this authority. Failure/ambiguous ACK keeps keeper custody live. */
typedef int (*ap_grouped_recovery_ack)(void *,const struct ap_grouped_owner *,
    const int [3],const struct ap_grouped_recovery_step *,size_t);
/* Never creates, joins leaves, activates or deletes. Only a fresh io/EMPTY
 * owner,1..17 authenticated attempted roles, and a current subset census may
 * become QUIESCENT after the real recovery ACK. Caller supplies the existing
 * absolute deadline; deletion separately retains original release_start+1s. */
int ap_grouped_io_adopt_recovery(struct ap_grouped_io *,struct ap_grouped_owner *,
    const struct ap_grouped_recovery_step *,size_t,ap_grouped_recovery_ack,void *,uint64_t);
int ap_grouped_io_bind(struct ap_grouped_io *,struct ap_grouped_owner *,uint64_t);
int ap_grouped_io_activate(struct ap_grouped_io *,struct ap_grouped_owner *,uint64_t);
int ap_grouped_io_health(struct ap_grouped_io *,const struct ap_grouped_owner *,uint64_t);
/* release_start is the ORIGINAL before-all-releases monotonic timestamp,
 * already recorded by the terminal owner. It cannot be moved to after link
 * closure. Missing/late/unknown proof returns failure with custody retained. */
/* Only after actual original controller terminal custody, no successful
 * provider admission, and closure of every partially acquired BPF owner.
 * Reconcile the already-adopted original full history; never reconstruct an
 * owner, claim a foreign tuple, or authorize a second startup attempt. The
 * caller retains the original failure and release_start across this call. */
int ap_grouped_io_recover_terminal(struct ap_grouped_io *,struct ap_grouped_owner *,uint64_t,uint64_t);
int ap_grouped_io_delete_until(struct ap_grouped_io *,struct ap_grouped_owner *,uint64_t,uint64_t);
int ap_grouped_io_delete(struct ap_grouped_io *,struct ap_grouped_owner *,uint64_t);
/* Descriptive results only. attempted/returned distinguish no call or unknown
 * outcome from a real returned value; zero-initialized result is not success.
 * Complete-read results describe the maintained bounded read helper, not one
 * native read. Directory results are actual fstatat returns/errno. */
struct ap_grouped_complete_read_observation {
    unsigned attempted,returned;
    ssize_t result;
    int error;
};
struct ap_grouped_directory_observation {
    unsigned attempted,returned;
    int result,error;
};
struct ap_grouped_absence_observation {
    uint64_t cutoff,started_ns,finished_ns;
    unsigned started_observed,finished_observed,complete;
    size_t definition_bytes,profile_bytes;
    struct ap_grouped_complete_read_observation definitions,profile;
    struct ap_grouped_directory_observation event_directory,group_directory;
};
/* One fresh full definition/profile/directory scan under the caller's unchanged
 * cutoff. Requires actual ABSENT with retained nonzero attempted ownership and
 * no verified/pending role. Owner/phase/unknown/writes are never modified, no
 * callback or write is issued, and no provider/terminal authority is created.
 * On success io->proof[0..definition_bytes] and io->buffer[0..profile_bytes]
 * retain these fresh exact bytes for the caller to copy/hash BEFORE another
 * operation uses those buffers. No historical proof/profile is accepted.
 * The caller must own the exclusive cross-process CONTROL/PROFILE OFD cursor
 * lease for the entire call and two sequential fresh scans. A raw fd, phase,
 * prior observation or timeout cannot supply that lease. A failed observation
 * stays a failed operation in its retained caller; a later read cannot clear it.
 * These bytes/observations cannot substitute for native no-provider/terminal
 * proofs, an actual live cleanup peer, original histories, or final actor/FD/ID
 * restoration. The current helper grants none of those capabilities. */
int ap_grouped_io_observe_absent(struct ap_grouped_io *,const struct ap_grouped_owner *,
    uint64_t,struct ap_grouped_absence_observation *);
#endif
