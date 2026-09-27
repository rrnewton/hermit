/* Both original retained owners must acknowledge before a global write. */
#ifndef HERMIT_GROUPED_KEEPER_DUAL_H
#define HERMIT_GROUPED_KEEPER_DUAL_H
#include "grouped-keeper-wire.h"

struct grouped_keeper_dual {
    struct grouped_keeper_wire *guardian,*keeper;
    uint64_t sequence,deadline;
    int refused,error;
    unsigned guardian_ack,keeper_ack;
};
/* The two already authenticated endpoints retain their actual peer pidfds and
 * descriptions. Their Python receivers construct JournalSession only after
 * receiving the actual creator/cgroup and control descriptions. Neither this
 * object nor a numeric pair of descriptors substitutes for that receiver. */
int grouped_keeper_dual_init(struct grouped_keeper_dual *,
    struct grouped_keeper_wire *,struct grouped_keeper_wire *);
int grouped_keeper_dual_shorten_deadline(struct grouped_keeper_dual *,uint64_t);
/* The mandatory ap_grouped_io journal callback. A partial delivery permanently
 * refuses this sender; it cannot overwrite either endpoint's retained final
 * packet or retry a sequence acknowledged by only one owner. */
int grouped_keeper_dual_journal(void *,const struct ap_grouped_owner *,
    const struct ap_grouped_write *,const char *);
#endif
