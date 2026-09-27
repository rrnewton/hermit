/* Source-only bootstrap transport. This header grants no provider authority. */
#ifndef HERMIT_GROUPED_KEEPER_WIRE_H
#define HERMIT_GROUPED_KEEPER_WIRE_H
#include "grouped-io.h"
#include <sys/socket.h>

#define GK_FRAME_BYTES 1536U
#define GK_ACK_BYTES 256U
struct grouped_keeper_wire {
    int channel,keeper_pidfd,refused;
    struct ucred keeper;
    uint64_t incarnation,deadline,sequence;
    char nonce[AP_GROUPED_NONCE_BYTES+1];
    char sent[GK_FRAME_BYTES],received[GK_ACK_BYTES];
    size_t sent_bytes,received_bytes;
    ssize_t send_return,receive_return;
    int send_error,receive_error,receive_flags;
};
/* Actual private socket SO_PEERCRED and a live matching pidfd authenticate the
 * keeper; a PID or ready boolean is not accepted. The caller retains socket
 * custody. This object retains its actual peer pidfd even on later refusal. */
int grouped_keeper_wire_init(struct grouped_keeper_wire *,int,uint64_t,
    const char *,uint64_t);
int grouped_keeper_wire_shorten_deadline(struct grouped_keeper_wire *,uint64_t);
/* Recheck the continuously held actual peer/socket and the same deadline. */
int grouped_keeper_wire_validate(struct grouped_keeper_wire *);
/* Pure exact serialization. No write, ACK or adoption occurs here. */
int grouped_keeper_journal_frame(char *,size_t,const char *,uint64_t,uint64_t,
    const struct ap_grouped_owner *,const struct ap_grouped_write *,const char *);
/* Mandatory before/after callback for ap_grouped_io_init. Raw request/reply
 * stay in this owner; an ambiguous send, malformed reply or timeout is sticky.
 * No retry and no ownership claim follows a callback failure. */
int grouped_keeper_journal(void *,const struct ap_grouped_owner *,
    const struct ap_grouped_write *,const char *);
#endif
