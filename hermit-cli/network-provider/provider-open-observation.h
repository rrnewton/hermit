/* Failure diagnostics do not grant another startup interval. */
#ifndef PROVIDER_OPEN_OBSERVATION_H
#define PROVIDER_OPEN_OBSERVATION_H
#include <errno.h>
#include <stdint.h>
enum nr_inventory_phase { NR_INVENTORY_NOT_STARTED, NR_INVENTORY_IDENTIFIERS,
    NR_INVENTORY_SAVE_IDS, NR_INVENTORY_METADATA, NR_INVENTORY_COMPLETE };
struct nr_open_observation {
    uint64_t version,start,deadline,run_deadline,returned,inventory_start,inventory_end,observed;
    int32_t open_rc,open_error,inventory_rc,inventory_error,phase,gate_error;
};
static inline int nr_open_timely(uint64_t instant,uint64_t startup,uint64_t run) {
    if(!instant || !startup || !run || instant>=startup || instant>=run) {errno=ETIMEDOUT;return -1;}
    return 0;
}
static inline int nr_open_observation_result(struct nr_open_observation *v,uint64_t instant,int has_session) {
    v->observed=instant;
    v->gate_error=nr_open_timely(v->returned,v->deadline,v->run_deadline)?ETIMEDOUT:0;
    if(nr_open_timely(instant,v->deadline,v->run_deadline))v->gate_error=ETIMEDOUT;
    int error=v->open_rc?v->open_error:v->inventory_rc?v->inventory_error:
        !has_session?EPROTO:v->gate_error;
    if(error || v->open_rc || v->inventory_rc) {errno=error?error:EPROTO;return -1;}
    return 0;
}
#endif
