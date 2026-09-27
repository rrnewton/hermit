/* No new process, deadline allowance or tracefs operation is introduced here. */
#define _GNU_SOURCE
#include "grouped-keeper-dual.h"
#include <errno.h>
#include <stdint.h>
#include <string.h>

static int refuse(struct grouped_keeper_dual *d,int error){
    if(d){d->refused=1;if(!d->error)d->error=error?error:EIO;errno=d->error;}
    else errno=EINVAL;
    return -1;
}
static int exact(struct grouped_keeper_dual *d){
    if(!d||d->refused)return refuse(d,d?d->error:EINVAL);
    struct grouped_keeper_wire *g=d->guardian,*k=d->keeper;
    if(!g||!k||g==k||g->keeper.pid==k->keeper.pid||g->channel==k->channel||
       !g->incarnation||g->incarnation!=k->incarnation||
       memcmp(g->nonce,k->nonce,sizeof(g->nonce))||
       g->deadline!=d->deadline||k->deadline!=d->deadline||
       g->sequence!=d->sequence||k->sequence!=d->sequence)
        return refuse(d,EINVAL);
    if(grouped_keeper_wire_validate(g)||grouped_keeper_wire_validate(k))return refuse(d,errno);
    return 0;
}
int grouped_keeper_dual_init(struct grouped_keeper_dual *d,
        struct grouped_keeper_wire *guardian,struct grouped_keeper_wire *keeper){
    if(!d){errno=EINVAL;return -1;}
    memset(d,0,sizeof(*d));d->guardian=guardian;d->keeper=keeper;d->sequence=1;
    if(!guardian||!keeper)return refuse(d,EINVAL);
    d->deadline=guardian->deadline;
    return exact(d);
}
int grouped_keeper_dual_shorten_deadline(struct grouped_keeper_dual *d,uint64_t deadline){
    if(!d||d->refused||!d->guardian||!d->keeper||!deadline||deadline>d->deadline)
        return refuse(d,EINVAL);
    /* Latch the original earlier cutoff on both endpoints before any fallible
     * check, including when it has expired. No fallback restarts a budget. */
    d->deadline=deadline;d->guardian->deadline=deadline;d->keeper->deadline=deadline;
    return exact(d);
}
int grouped_keeper_dual_journal(void *context,const struct ap_grouped_owner *owner,
        const struct ap_grouped_write *write,const char *line){
    struct grouped_keeper_dual *d=context;
    if(exact(d))return -1;
    if(d->sequence==UINT64_MAX)return refuse(d,EOVERFLOW);
    d->guardian_ack=d->keeper_ack=0;
    /* The existing ap_grouped_io caller cannot reach write(2) unless this
     * function returns0. On an outcome callback, both owners already retain
     * the corresponding intent even if only this first outcome ACK arrives. */
    if(grouped_keeper_journal(d->guardian,owner,write,line))return refuse(d,errno);
    d->guardian_ack=1;
    if(grouped_keeper_journal(d->keeper,owner,write,line))return refuse(d,errno);
    d->keeper_ack=1;
    d->sequence++;
    /* Retention ACKs do not turn a short/error native return into success.
     * Both owners now have the same failed outcome; no later callback may
     * overwrite it or issue a new write through this pair. */
    if(write->completed && (write->raw<0 || (size_t)write->raw!=write->submitted))
        return refuse(d,write->raw<0?write->error:EIO);
    return exact(d);
}
