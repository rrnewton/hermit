/* Harness prerequisite only; this API neither creates probes nor opens BPF. */
#ifndef HERMIT_GROUPED_GUARDIAN_BOOTSTRAP_H
#define HERMIT_GROUPED_GUARDIAN_BOOTSTRAP_H
#include "grouped-keeper-wire.h"

struct grouped_guardian_bootstrap {
    struct grouped_keeper_wire *keeper;
    struct grouped_keeper_wire guardian;
    int creator_pidfd,cgroup_directory,refused,error;
    uint64_t creator_cutoff;
    char packet[2048];size_t packet_bytes;
    ssize_t receive_return,send_return;
    int receive_flags;
    unsigned endpoint_received,creator_acknowledged,controls_acknowledged;
};
/* original_creator_cutoff is the existing BEFORE-first-creator-receipt +1s
 * cutoff, also bounded by the original stage allowance. It is not refreshed
 * after the first keeper ACK. Failure retains this object's actual resources.
 */
int grouped_guardian_bootstrap_init(struct grouped_guardian_bootstrap *,
    struct grouped_keeper_wire *,uint64_t original_creator_cutoff);
int grouped_guardian_bootstrap_creator(struct grouped_guardian_bootstrap *,const char *unit);
/* Exact original manager controls, not a caller-created control object. The
 * two retained receivers independently validate roles and keep real SCM
 * duplicates before their READY ACK. Call only after both creator captures.
 */
int grouped_guardian_bootstrap_controls(struct grouped_guardian_bootstrap *,const int [3]);
#endif
