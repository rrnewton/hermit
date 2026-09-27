/* SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include "grouped-cleanup-bridge.h"
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

struct cleanup_absence_slot {
    struct ap_grouped_absence_observation observation;
    unsigned char *definitions,*profile;
    size_t definition_bytes,profile_bytes;
    unsigned definition_bytes_known,profile_bytes_known;
};
struct hermit_grouped_cleanup {
    struct hermit_grouped_cleanup_status status;
    struct ap_grouped_owner owner;
    struct ap_grouped_io io;
    struct ap_grouped_recovery_step steps[AP_GROUPED_SITE_COUNT];
    struct hermit_grouped_cleanup_callback callbacks[1+AP_GROUPED_SITE_COUNT*2];
    struct cleanup_absence_slot observations[2];
    ap_grouped_journal journal;
    void *journal_context;
    ap_grouped_recovery_ack acknowledge;
    void *acknowledge_context;
    unsigned active;
};
_Static_assert(sizeof(struct hermit_grouped_cleanup)<=HERMIT_GROUPED_CLEANUP_CONTEXT_BYTES,
    "cleanup retained context exceeds fixed bound");
static int hc_invalid(void) {errno=EINVAL;return -1;}
static void hc_observed(struct hermit_grouped_cleanup_call *call,int raw,int error) {
    call->raw=raw;call->error=raw?error:0;call->returned=1;
}
static int hc_clock_sample(struct hermit_grouped_cleanup_call *call,uint64_t *out,int *validation) {
    struct timespec ts;call->attempted=1;
    int raw=clock_gettime(CLOCK_MONOTONIC,&ts),error=raw?errno:0;hc_observed(call,raw,error);
    if(raw){errno=error;return -1;}
    if(ts.tv_sec<0 || ts.tv_nsec<0 || ts.tv_nsec>=1000000000L ||
       (uint64_t)ts.tv_sec>UINT64_MAX/1000000000ULL ||
       (uint64_t)ts.tv_sec*1000000000ULL>UINT64_MAX-(uint64_t)ts.tv_nsec) {
        *validation=EPROTO;errno=EPROTO;return -1;
    }
    *out=(uint64_t)ts.tv_sec*1000000000ULL+(uint64_t)ts.tv_nsec;return 0;
}
static int hc_now_ns(uint64_t *out) {
    struct hermit_grouped_cleanup_call call={0};int validation=0;
    return hc_clock_sample(&call,out,&validation);
}
static int hc_refuse(struct hermit_grouped_cleanup *b,int error) {
    if(!b)return hc_invalid();
    if(!b->status.refused) {
        b->status.refused=1;b->status.first_error=error?error:EPROTO;
        /* Exactly this first failure samples a local bound. Clock failure stays
         * unknown; later status/cleanup cannot replace it with a newer origin. */
        if(!hc_clock_sample(&b->status.local_failure_clock,&b->status.local_failure_origin,
            &b->status.local_failure_clock_validation_error))b->status.local_failure_origin_valid=1;
    }
    errno=b->status.first_error;return -1;
}
static int hc_complete_call(struct hermit_grouped_cleanup *b,struct hermit_grouped_cleanup_call *call,
        int raw,int error) {
    if(raw || b->status.refused) {hc_refuse(b,raw?error:b->status.first_error);raw=-1;error=errno;}
    hc_observed(call,raw,error);b->active=0;
    if(raw)errno=error;
    return raw;
}
static int hc_begin(struct hermit_grouped_cleanup *b,struct hermit_grouped_cleanup_call *call) {
    if(call->attempted)return hc_refuse(b,EALREADY);
    call->attempted=1;
    if(b->active || b->status.refused || b->status.aliases_attempted) {
        hc_refuse(b,b->active?EBUSY:EINVAL);hc_observed(call,-1,errno);return -1;
    }
    b->active=1;return 0;
}
static uint64_t hc_minimum(uint64_t a,uint64_t b) {return a<b?a:b;}
static int hc_within(uint64_t cutoff) {
    uint64_t now;if(hc_now_ns(&now))return -1;
    if(!cutoff || now>=cutoff){errno=ETIMEDOUT;return -1;}
    return 0;
}
static int hc_alias_cutoff(struct hermit_grouped_cleanup *b,uint64_t *out) {
    uint64_t cutoff=b->status.original_retained?b->status.effective_cutoff:b->status.stage_cutoff;
    if(!cutoff)return hc_invalid();
    if(b->status.refused) {
        if(!b->status.local_failure_origin_valid || !b->status.local_failure_origin ||
           b->status.local_failure_origin>UINT64_MAX-1000000000ULL)return hc_invalid();
        uint64_t local=b->status.local_failure_origin+1000000000ULL;
        cutoff=hc_minimum(cutoff,local);
    }
    if(!cutoff)return hc_invalid();
    *out=cutoff;return hc_within(cutoff);
}
unsigned hermit_grouped_cleanup_abi(void) {return HERMIT_GROUPED_CLEANUP_ABI;}
int hermit_grouped_cleanup_alloc(struct hermit_grouped_cleanup **out) {
    if(!out || *out)return hc_invalid();
    struct hermit_grouped_cleanup *b=calloc(1,sizeof(*b));if(!b)return -1;
    for(unsigned i=0;i<AP_GROUP_FDS;i++){b->io.fd[i]=-1;b->status.closes[i].descriptor=-1;}
    b->status.abi=HERMIT_GROUPED_CLEANUP_ABI;b->status.allocation.attempted=1;*out=b;
    for(unsigned i=0;i<2;i++) {
        b->observations[i].definitions=malloc(AP_GROUPED_CENSUS_BYTES);
        if(!b->observations[i].definitions)return hc_complete_call(b,&b->status.allocation,-1,errno);
        b->status.receipt_buffers_allocated+=AP_GROUPED_CENSUS_BYTES;
        b->observations[i].profile=malloc(AP_GROUPED_CENSUS_BYTES);
        if(!b->observations[i].profile)return hc_complete_call(b,&b->status.allocation,-1,errno);
        b->status.receipt_buffers_allocated+=AP_GROUPED_CENSUS_BYTES;
    }
    b->status.allocation_ready=1;return hc_complete_call(b,&b->status.allocation,0,0);
}
static struct hermit_grouped_cleanup_callback *hc_callback_slot(struct hermit_grouped_cleanup *b,unsigned kind) {
    if(!b->active || b->status.callbacks_count>=1+AP_GROUPED_SITE_COUNT*2 ||
       (kind==1 && (!b->status.io_adopt.attempted || b->status.io_adopt.returned || b->status.callbacks_count)) ||
       (kind==2 && (!b->status.io_delete.attempted || b->status.io_delete.returned ||
        !b->status.adopted || !b->status.callbacks_count)) || (kind!=1 && kind!=2)) {
        hc_refuse(b,EPROTO);return NULL;
    }
    struct hermit_grouped_cleanup_callback *row=&b->callbacks[b->status.callbacks_count++];
    row->kind=kind;row->return_to_io.attempted=1;return row;
}
static int hc_callback_return(struct hermit_grouped_cleanup *b,struct hermit_grouped_cleanup_callback *row,
        int raw,int error) {
    if(raw || b->status.refused){hc_refuse(b,raw?error:b->status.first_error);raw=-1;error=errno;}
    hc_observed(&row->return_to_io,raw,error);
    if(raw)errno=error;
    return raw;
}
static int hc_cleanup_journal(void *context,const struct ap_grouped_owner *owner,
        const struct ap_grouped_write *write,const char *line) {
    struct hermit_grouped_cleanup *b=context;
    struct hermit_grouped_cleanup_callback *row=hc_callback_slot(b,2);if(!row)return -1;
    if(!owner || !write || !line || write->remove!=1 || !write->submitted ||
       write->submitted>=AP_GROUPED_LINE_BYTES || !b->journal)
        return hc_callback_return(b,row,-1,EPROTO);
    row->owner=*owner;row->write=*write;row->line_bytes=write->submitted;
    memcpy(row->line,line,write->submitted);
    if(hc_within(b->status.effective_cutoff))return hc_callback_return(b,row,-1,errno);
    row->user_call.attempted=1;errno=0;
    int raw=b->journal(b->journal_context,owner,write,line),error=raw?errno:0;
    hc_observed(&row->user_call,raw,error);
    if(raw)return hc_callback_return(b,row,-1,error);
    if(hc_within(b->status.effective_cutoff))return hc_callback_return(b,row,-1,errno);
    return hc_callback_return(b,row,0,0);
}
int hermit_grouped_cleanup_prepare(struct hermit_grouped_cleanup *b,uint64_t incarnation,
        const char *nonce,size_t nonce_bytes,const int controls[3],uint64_t stage_cutoff,
        ap_grouped_journal journal,void *context) {
    if(!b)return hc_invalid();
    if(hc_begin(b,&b->status.prepare))return -1;
    b->status.incarnation=incarnation;b->status.stage_cutoff=stage_cutoff;
    b->journal=journal;b->journal_context=context;
    uint64_t now;
    if(!b->status.allocation_ready || !controls || !journal ||
       ap_grouped_owner_init(&b->owner,incarnation,nonce,nonce_bytes))
        return hc_complete_call(b,&b->status.prepare,-1,EINVAL);
    if(hc_now_ns(&now))return hc_complete_call(b,&b->status.prepare,-1,errno);
    if(!stage_cutoff || stage_cutoff<=now || stage_cutoff-now>20000000000ULL)
        return hc_complete_call(b,&b->status.prepare,-1,ETIMEDOUT);
    b->status.io_initialize.attempted=1;
    int raw=ap_grouped_io_init(&b->io,controls,hc_cleanup_journal,b),error=raw?errno:0;
    hc_observed(&b->status.io_initialize,raw,error);
    if(raw)return hc_complete_call(b,&b->status.prepare,-1,error);
    if(hc_within(stage_cutoff))return hc_complete_call(b,&b->status.prepare,-1,errno);
    b->status.prepared=1;return hc_complete_call(b,&b->status.prepare,0,0);
}
static int hc_pin_release(struct hermit_grouped_cleanup *b,const struct hermit_grouped_cleanup_release *original) {
    if(!original)return hc_invalid();
    b->status.original=*original;b->status.original_retained=1;
    /* Retain every numerically usable earlier bound before validation/clock
     * can fail. A zero enclosing cutoff stays zero, never a stage fallback. */
    b->status.effective_cutoff=hc_minimum(b->status.stage_cutoff,original->enclosing_cutoff);
    if(original->release_start && original->release_start<=UINT64_MAX-1000000000ULL)
        b->status.effective_cutoff=hc_minimum(b->status.effective_cutoff,original->release_start+1000000000ULL);
    if(original->has_first_failure==1 && original->first_failure_origin && original->first_failure_origin<=UINT64_MAX-1000000000ULL)
        b->status.effective_cutoff=hc_minimum(b->status.effective_cutoff,original->first_failure_origin+1000000000ULL);
    uint64_t now;if(hc_now_ns(&now))return -1;
    if(!original->release_start || original->release_start>now ||
       original->release_start>UINT64_MAX-1000000000ULL || original->has_first_failure>1 ||
       (!original->has_first_failure && original->first_failure_origin) ||
       (original->has_first_failure && (!original->first_failure_origin || original->first_failure_origin>now ||
        original->first_failure_origin>UINT64_MAX-1000000000ULL)))return hc_invalid();
    return hc_within(b->status.effective_cutoff);
}
static int hc_cleanup_ack(void *context,const struct ap_grouped_owner *owner,const int controls[3],
        const struct ap_grouped_recovery_step *steps,size_t count) {
    struct hermit_grouped_cleanup *b=context;
    struct hermit_grouped_cleanup_callback *row=hc_callback_slot(b,1);if(!row)return -1;
    if(!owner || !controls || steps!=b->steps || count!=b->status.retained_steps || !b->acknowledge)
        return hc_callback_return(b,row,-1,EPROTO);
    row->owner=*owner;
    for(unsigned i=0;i<3;i++)if(controls[i]!=b->io.fd[i])return hc_callback_return(b,row,-1,EPROTO);
    if(hc_within(b->status.effective_cutoff))return hc_callback_return(b,row,-1,errno);
    row->user_call.attempted=1;errno=0;
    int raw=b->acknowledge(b->acknowledge_context,owner,controls,steps,count),error=raw?errno:0;
    hc_observed(&row->user_call,raw,error);
    if(raw)return hc_callback_return(b,row,-1,error);
    if(hc_within(b->status.effective_cutoff))return hc_callback_return(b,row,-1,errno);
    return hc_callback_return(b,row,0,0);
}
int hermit_grouped_cleanup_adopt(struct hermit_grouped_cleanup *b,
        const struct ap_grouped_recovery_step *steps,size_t count,ap_grouped_recovery_ack acknowledge,
        void *context,const struct hermit_grouped_cleanup_release *original) {
    if(!b)return hc_invalid();
    if(hc_begin(b,&b->status.adopt))return -1;
    b->status.supplied_steps=count;b->acknowledge=acknowledge;b->acknowledge_context=context;
    if(steps && count && count<=AP_GROUPED_SITE_COUNT) {
        memcpy(b->steps,steps,count*sizeof(*steps));b->status.retained_steps=count;
    }
    int pinned=hc_pin_release(b,original),error=pinned?errno:0;
    if(pinned)return hc_complete_call(b,&b->status.adopt,-1,error);
    if(!b->status.prepared || !steps || !count || count>AP_GROUPED_SITE_COUNT || !acknowledge)
        return hc_complete_call(b,&b->status.adopt,-1,EINVAL);
    b->status.io_adopt.attempted=1;
    int raw=ap_grouped_io_adopt_recovery(&b->io,&b->owner,b->steps,count,hc_cleanup_ack,b,b->status.effective_cutoff);
    error=raw?errno:0;hc_observed(&b->status.io_adopt,raw,error);b->status.adopted=raw==0;
    if(raw)return hc_complete_call(b,&b->status.adopt,-1,error);
    if(hc_within(b->status.effective_cutoff))return hc_complete_call(b,&b->status.adopt,-1,errno);
    return hc_complete_call(b,&b->status.adopt,0,0);
}
int hermit_grouped_cleanup_delete(struct hermit_grouped_cleanup *b) {
    if(!b)return hc_invalid();
    if(hc_begin(b,&b->status.deletion))return -1;
    if(!b->status.adopted || b->owner.phase!=AP_GROUPED_QUIESCENT)
        return hc_complete_call(b,&b->status.deletion,-1,EINVAL);
    if(hc_within(b->status.effective_cutoff))return hc_complete_call(b,&b->status.deletion,-1,errno);
    b->status.io_delete.attempted=1;
    int raw=ap_grouped_io_delete_until(&b->io,&b->owner,b->status.original.release_start,b->status.effective_cutoff),error=raw?errno:0;
    hc_observed(&b->status.io_delete,raw,error);b->status.deleted=raw==0;
    if(raw)return hc_complete_call(b,&b->status.deletion,-1,error);
    if(hc_within(b->status.effective_cutoff))return hc_complete_call(b,&b->status.deletion,-1,errno);
    return hc_complete_call(b,&b->status.deletion,0,0);
}
static void hc_copy_observation(struct hermit_grouped_cleanup *b,unsigned index) {
    struct cleanup_absence_slot *slot=&b->observations[index];
    struct ap_grouped_absence_observation *o=&slot->observation;
    if(o->definitions.returned && o->definitions.result>=0 &&
       (uint64_t)o->definitions.result<=AP_GROUPED_CENSUS_BYTES) {
        slot->definition_bytes=(size_t)o->definitions.result;slot->definition_bytes_known=1;
        const char *bytes=o->profile.attempted?b->io.proof:b->io.buffer;
        memcpy(slot->definitions,bytes,slot->definition_bytes);
    }
    if(o->profile.returned && o->profile.result>=0 && (uint64_t)o->profile.result<=AP_GROUPED_CENSUS_BYTES) {
        slot->profile_bytes=(size_t)o->profile.result;slot->profile_bytes_known=1;
        memcpy(slot->profile,b->io.buffer,slot->profile_bytes);
    }
}
int hermit_grouped_cleanup_observe_absent(struct hermit_grouped_cleanup *b) {
    if(!b)return hc_invalid();
    if(b->status.absence_attempts>=2)return hc_refuse(b,EALREADY);
    unsigned index=b->status.absence_attempts++;
    if(hc_begin(b,&b->status.absence[index]))return -1;
    if(!b->status.deleted || b->owner.phase!=AP_GROUPED_ABSENT)
        return hc_complete_call(b,&b->status.absence[index],-1,EINVAL);
    if(hc_within(b->status.effective_cutoff))return hc_complete_call(b,&b->status.absence[index],-1,errno);
    b->status.io_absence[index].attempted=1;
    int raw=ap_grouped_io_observe_absent(&b->io,&b->owner,b->status.effective_cutoff,&b->observations[index].observation),error=raw?errno:0;
    hc_observed(&b->status.io_absence[index],raw,error);hc_copy_observation(b,index);
    if(raw)return hc_complete_call(b,&b->status.absence[index],-1,error);
    if(hc_within(b->status.effective_cutoff))return hc_complete_call(b,&b->status.absence[index],-1,errno);
    b->status.absence_complete_mask|=1U<<index;
    return hc_complete_call(b,&b->status.absence[index],0,0);
}
int hermit_grouped_cleanup_status(const struct hermit_grouped_cleanup *b,struct hermit_grouped_cleanup_status *out) {
    if(!b || !out || b->active)return hc_invalid();
    *out=b->status;out->owner=b->owner;out->writes_count=b->io.writes_count;
    memcpy(out->descriptors,b->io.fd,sizeof(out->descriptors));
    out->io_buffer_present=b->io.buffer!=NULL;out->io_proof_present=b->io.proof!=NULL;return 0;
}
int hermit_grouped_cleanup_history(const struct hermit_grouped_cleanup *b,struct hermit_grouped_cleanup_history *out) {
    if(!b || !out || b->active)return hc_invalid();
    *out=(struct hermit_grouped_cleanup_history){.owner=b->owner,.writes_count=b->io.writes_count,
        .retained_steps=b->status.retained_steps,.callbacks_count=b->status.callbacks_count};
    memcpy(out->writes,b->io.writes,sizeof(out->writes));memcpy(out->steps,b->steps,sizeof(out->steps));
    memcpy(out->callbacks,b->callbacks,sizeof(out->callbacks));return 0;
}
int hermit_grouped_cleanup_absence(const struct hermit_grouped_cleanup *b,unsigned index,struct hermit_grouped_cleanup_absence *out) {
    if(!b || !out || b->active || index>=b->status.absence_attempts || index>=2)return hc_invalid();
    const struct cleanup_absence_slot *s=&b->observations[index];
    *out=(struct hermit_grouped_cleanup_absence){.call=b->status.absence[index],.io_call=b->status.io_absence[index],
        .observation=s->observation,.definition_bytes_known=s->definition_bytes_known,.profile_bytes_known=s->profile_bytes_known,
        .definition_bytes=s->definition_bytes,.profile_bytes=s->profile_bytes,.definitions=s->definitions,.profile=s->profile};
    return 0;
}
int hermit_grouped_cleanup_release_aliases(struct hermit_grouped_cleanup *b) {
    if(!b)return hc_invalid();
    if(b->active || b->status.aliases_attempted)return hc_refuse(b,b->active?EBUSY:EALREADY);
    b->status.aliases_attempted=1;b->status.aliases.attempted=1;b->active=1;
    int error=0;uint64_t cutoff=0;
    for(unsigned i=AP_GROUP_FDS;i;i--)if(b->io.fd[i-1]>=0) {
        if(hc_alias_cutoff(b,&cutoff)){error=errno;break;}
        struct hermit_grouped_cleanup_close *row=&b->status.closes[i-1];
        row->descriptor=b->io.fd[i-1];b->io.fd[i-1]=-1;row->call.attempted=1;
        int raw=close(row->descriptor),saved=raw?errno:0;hc_observed(&row->call,raw,saved);
        if(raw){hc_refuse(b,saved);if(!error)error=saved;}
        if(hc_within(cutoff)){if(!error)error=errno;break;}
    }
    if(error)hc_refuse(b,error);
    int held=0;for(unsigned i=0;i<AP_GROUP_FDS;i++)held|=b->io.fd[i]>=0;
    if(!held) {
        free(b->io.buffer);b->io.buffer=NULL;free(b->io.proof);b->io.proof=NULL;
        b->status.aliases_returned=1;
    }
    hc_observed(&b->status.aliases,error?-1:0,error);b->active=0;
    if(error){errno=error;return -1;}return 0;
}
int hermit_grouped_cleanup_free(struct hermit_grouped_cleanup **owned) {
    if(!owned || !*owned)return hc_invalid();
    struct hermit_grouped_cleanup *b=*owned;
    if(b->active || !b->status.aliases_returned)return hc_refuse(b,EINVAL);
    for(unsigned i=0;i<AP_GROUP_FDS;i++)if(b->io.fd[i]>=0)return hc_refuse(b,EBUSY);
    *owned=NULL;
    for(unsigned i=0;i<2;i++){free(b->observations[i].definitions);free(b->observations[i].profile);}
    free(b);return 0;
}
