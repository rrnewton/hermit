/* SPDX-License-Identifier: MIT */
#ifndef AP_TASK_DISARM_H
#define AP_TASK_DISARM_H
#include <stdbool.h>
/* Driver-private diagnostics on the existing pending command, not wire ABI.
 * Keep the successful mutation distinct from a failed verification read. */
enum ap_task_disarm_phase {
    AP_DISARM_NONE,
    AP_DISARM_IDLE_UPDATE_RETURNED,
    AP_DISARM_IDLE_READBACK_RETURNED,
    AP_DISARM_DEAD_DELETE_RETURNED,
    AP_DISARM_DEAD_READBACK_RETURNED,
};
struct ap_task_disarm_outcome {
    enum ap_task_disarm_phase phase;
    int mutation_rc,mutation_errno,readback_rc,readback_errno; /* errno is meaningful on failure only */
    bool detached_verified;
};
#endif
