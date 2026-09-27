/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#ifndef HERMIT_GROUPED_ADOPTION_WIRE_H
#define HERMIT_GROUPED_ADOPTION_WIRE_H
#include "grouped-keeper-wire.h"
#define GA_TRANSFER_BYTES 65536U
#define GA_REPLY_BYTES 2048U
struct grouped_adoption_wire {
    struct grouped_keeper_wire *keeper;
    struct ap_grouped_creation_pair pairs[AP_GROUPED_SITE_COUNT];
    struct ap_grouped_owner expected;
    int controls[3],initialized,refused,error;
    unsigned receive_attempted,received,ack_attempted,acknowledged;
    char offer[33],transcript_sha256[65];
    char packet[GA_TRANSFER_BYTES],expected_packet[GA_TRANSFER_BYTES];
    char request[GA_REPLY_BYTES],reply[GA_REPLY_BYTES];
    size_t packet_bytes,request_bytes,reply_bytes;
    ssize_t receive_return,send_return,ack_return;
    int receive_flags,ack_flags,receive_error,send_error,ack_error;
    unsigned receive_called,ack_called;
};
/* Pure canonical codec. Reconstructs every exact17 successful pending owner /
 * intent / outcome pair; no serializer grants custody or native authority. */
int grouped_created_transfer_frame(char *,size_t,struct ap_grouped_creation_pair *,
    struct ap_grouped_owner *,char [65],uint64_t,const char *,const char *);
/* Caller supplies fresh zero-initialized storage and a continuously retained
 * authenticated keeper. Received controls remain owned even after refusal.
 * This does not validate tracefs roles: actual ap_grouped_io_init must do so
 * before passing its duplicates to the real ap_grouped_io_adopt_created API. */
int grouped_adoption_wire_init(struct grouped_adoption_wire *,struct grouped_keeper_wire *);
int grouped_adoption_wire_receive(struct grouped_adoption_wire *);
/* Actual adoption callback only after the API's full FSM/census check. Echoes
 * real SCM duplicates of the same descriptions, then requires the keeper's
 * one-use authority ACK. Ambiguous sends/ACKs remain sticky, never retryable.
 * The keeper separately proves S1 terminal, S2 credentials/cgroup and original
 * retained controls; this client cannot manufacture that proof. */
int grouped_adoption_wire_ack(void *,const struct ap_grouped_owner *,const int [3],
    const struct ap_grouped_creation_pair *,size_t);
/* Releases only this client's received aliases. It never deletes global state,
 * retries an ambiguous close, or claims original-deadline global absence. */
int grouped_adoption_wire_release(struct grouped_adoption_wire *);
#endif
