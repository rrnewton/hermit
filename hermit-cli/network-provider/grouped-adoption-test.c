/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
/* Exercise the actual read-only adoption API with ordinary temporary files.
 * The controlled callback is not keeper, SCM, pidfd or tracefs authority. */
#define _GNU_SOURCE
#include "grouped-io.c"
#include <assert.h>
static const char nonce[]="1234567890abcdef1234567890abcdef";
struct fixture {
    struct ap_grouped_owner owner;
    struct ap_grouped_io io;
    struct ap_grouped_creation_pair pairs[AP_GROUPED_SITE_COUNT];
    FILE *control,*profile,*events;
    unsigned acks,journals,action;
    uint64_t cutoff;
};
static void replace(FILE *f,const void *bytes,size_t n) {
    assert(!fflush(f));assert(!ftruncate(fileno(f),0));assert(!fseek(f,0,SEEK_SET));
    assert(fwrite(bytes,1,n,f)==n);assert(!fflush(f));
}
static void projection(struct fixture *f,uint32_t mask) {
    char definitions[AP_GROUPED_SITE_COUNT*AP_GROUPED_LINE_BYTES],profile[4096];
    size_t d=0,p=0;
    for(unsigned role=1;role<=AP_GROUPED_SITE_COUNT;role++)if(mask&(1U<<(role-1))) {
        int n=ap_grouped_command(&f->owner,role,0,definitions+d,sizeof(definitions)-d);assert(n>0);d+=(size_t)n;
        n=snprintf(profile+p,sizeof(profile)-p,"  %-44s %15llu %15llu\n",f->owner.event,0ULL,0ULL);
        assert(n>0 && (size_t)n<sizeof(profile)-p);p+=(size_t)n;
    }
    replace(f->control,definitions,d);replace(f->profile,profile,p);
}
static int journal(void *opaque,const struct ap_grouped_owner *owner,
        const struct ap_grouped_write *write,const char *line) {
    struct fixture *f=opaque;(void)owner;(void)write;(void)line;f->journals++;assert(0);return -1;
}
static int ack(void *opaque,const struct ap_grouped_owner *owner,const int fd[3],
        const struct ap_grouped_creation_pair *pairs,size_t count) {
    struct fixture *f=opaque;f->acks++;
    assert(owner->phase==AP_GROUPED_CREATED && owner->attempted_sites==AP_GROUPED_ALL_SITES);
    assert(owner->verified_sites==AP_GROUPED_ALL_SITES && !owner->write_unknown && !owner->pending_role);
    assert(pairs==f->pairs && count==AP_GROUPED_SITE_COUNT);
    assert(fd[0]==fileno(f->control)&&fd[1]==fileno(f->profile)&&fd[2]==fileno(f->events));
    assert(f->owner.phase==AP_GROUPED_EMPTY && !f->owner.attempted_sites && !f->io.writes_count);
    if(f->action==1)return -1;
    if(f->action==2)projection(f,AP_GROUPED_ALL_SITES&~1U);
    if(f->action==3)replace(f->profile,"invalid\n",8);
    if(f->action==4) {
        const uint64_t wake=f->cutoff+1000000ULL;
        struct timespec until={.tv_sec=(time_t)(wake/1000000000ULL),.tv_nsec=(long)(wake%1000000000ULL)};
        assert(!clock_nanosleep(CLOCK_MONOTONIC,TIMER_ABSTIME,&until,NULL));
    }
    return 0;
}
static void init(struct fixture *f) {
    *f=(struct fixture){0};assert(!ap_grouped_owner_init(&f->owner,41,nonce,32));
    struct ap_grouped_owner current=f->owner;assert(!ap_grouped_owner_ack(&current,41,nonce,32));
    char history[AP_GROUPED_SITE_COUNT*AP_GROUPED_LINE_BYTES];size_t at=0;
    for(unsigned role=1;role<=AP_GROUPED_SITE_COUNT;role++) {
        struct ap_grouped_creation_pair *p=&f->pairs[role-1];
        int n=ap_grouped_create_begin(&current,history,at,role,p->line,sizeof(p->line));assert(n>0);
        p->intent_owner=p->outcome_owner=current;
        p->intent=(struct ap_grouped_write){.role=role,.submitted=(size_t)n};
        p->outcome=p->intent;p->outcome.started=p->outcome.completed=1;p->outcome.raw=n;
        memcpy(history+at,p->line,(size_t)n);at+=(size_t)n;
        assert(!ap_grouped_create_observed(&current,role,n,(size_t)n,history,at));
    }
    f->control=tmpfile();f->profile=tmpfile();f->events=tmpfile();assert(f->control&&f->profile&&f->events);
    for(unsigned i=0;i<AP_GROUP_FDS;i++)f->io.fd[i]=-1;
    f->io.fd[0]=fileno(f->control);f->io.fd[1]=fileno(f->profile);f->io.fd[2]=fileno(f->events);
    f->io.buffer=malloc(AP_GROUPED_CENSUS_BYTES+1);f->io.proof=malloc(AP_GROUPED_CENSUS_BYTES+1);
    assert(f->io.buffer&&f->io.proof);f->io.journal=journal;f->io.journal_context=f;
    projection(f,AP_GROUPED_ALL_SITES);
}
static void done(struct fixture *f) {
    assert(!f->journals);assert(!fclose(f->control));assert(!fclose(f->profile));assert(!fclose(f->events));
    free(f->io.buffer);free(f->io.proof);
}
static uint64_t deadline(void) {uint64_t now;assert(!now_ns(&now));return now+1000000000ULL;}
static void ledger(const struct fixture *f) {
    assert(f->owner.attempted_sites==AP_GROUPED_ALL_SITES && f->owner.verified_sites==AP_GROUPED_ALL_SITES);
    assert(f->io.writes_count==AP_GROUPED_SITE_COUNT && !f->owner.write_unknown);
    for(unsigned i=0;i<AP_GROUPED_SITE_COUNT;i++)assert(!memcmp(&f->io.writes[i],&f->pairs[i].outcome,sizeof(f->io.writes[i])));
}
static void full_adoption(void) {
    struct fixture f;init(&f);
    assert(!ap_grouped_io_adopt_created(&f.io,&f.owner,f.pairs,AP_GROUPED_SITE_COUNT,ack,&f,deadline()));
    assert(f.acks==1 && f.owner.phase==AP_GROUPED_CREATED);ledger(&f);
    assert(ap_grouped_io_adopt_created(&f.io,&f.owner,f.pairs,AP_GROUPED_SITE_COUNT,ack,&f,deadline()));
    assert(f.acks==1 && f.owner.phase==AP_GROUPED_CREATED);ledger(&f);
    assert(ap_grouped_io_bind(&f.io,&f.owner,deadline()));
    assert(ap_grouped_io_activate(&f.io,&f.owner,deadline()));done(&f);
}
static void every_site_history_refusals(void) {
    for(unsigned site=0;site<AP_GROUPED_SITE_COUNT;site++)for(unsigned variant=0;variant<30;variant++) {
        struct fixture f;init(&f);struct ap_grouped_creation_pair *p=&f.pairs[site];
        switch(variant) {
        case 0:p->intent_owner.incarnation++;break;
        case 1:p->intent_owner.group[8]='x';break;
        case 2:p->intent_owner.event[0]='x';break;
        case 3:p->intent_owner.phase=AP_GROUPED_CREATED;break;
        case 4:p->intent_owner.verified_sites^=1;break;
        case 5:p->intent_owner.attempted_sites^=1;break;
        case 6:p->intent_owner.event_id=1;break;
        case 7:p->intent_owner.write_unknown=1;break;
        case 8:p->intent_owner.pending_role=0;break;
        case 9:p->intent_owner.pending_remove=1;break;
        case 10:p->intent_owner.pending_bytes++;break;
        case 11:p->outcome_owner.pending_role=0;break;
        case 12:p->intent.role=0;break;
        case 13:p->intent.remove=1;break;
        case 14:p->intent.submitted++;break;
        case 15:p->intent.error=EIO;break;
        case 16:p->intent.started=1;break;
        case 17:p->intent.completed=1;break;
        case 18:p->intent.raw=1;break;
        case 19:p->outcome.role=0;break;
        case 20:p->outcome.remove=1;break;
        case 21:p->outcome.submitted++;break;
        case 22:p->outcome.error=EIO;break;
        case 23:p->outcome.started=0;break;
        case 24:p->outcome.completed=0;break;
        case 25:p->outcome.raw--;break;
        case 26:p->line[0]='r';break;
        case 27:p->line[p->intent.submitted]='x';break;
        case 28:p->line[p->intent.submitted+1]='x';break;
        case 29:*p=f.pairs[(site+1)%AP_GROUPED_SITE_COUNT];break;
        default:assert(0);
        }
        assert(ap_grouped_io_adopt_created(&f.io,&f.owner,f.pairs,AP_GROUPED_SITE_COUNT,ack,&f,deadline()));
        assert(!f.acks && !f.owner.attempted_sites && !f.io.writes_count);
        assert(f.owner.phase==AP_GROUPED_UNKNOWN);done(&f);
    }
}
static void admission_refusals(void) {
    for(unsigned variant=0;variant<7;variant++) {
        struct fixture f;init(&f);size_t count=AP_GROUPED_SITE_COUNT;uint64_t end=deadline();
        switch(variant) {
        case 0:count=0;break;
        case 1:count--;break;
        case 2:count++;break;
        case 3:end=1;break;
        case 4:projection(&f,AP_GROUPED_ALL_SITES&~1U);break;
        case 5:replace(f.profile,"invalid\n",8);break;
        case 6:projection(&f,0);break;
        default:assert(0);
        }
        assert(ap_grouped_io_adopt_created(&f.io,&f.owner,f.pairs,count,ack,&f,end));
        assert(!f.acks && !f.owner.attempted_sites && !f.io.writes_count);done(&f);
    }
}
static void ack_and_late_failure_custody(void) {
    for(unsigned action=1;action<=4;action++) {
        struct fixture f;init(&f);f.action=action;f.cutoff=deadline();
        assert(ap_grouped_io_adopt_created(&f.io,&f.owner,f.pairs,AP_GROUPED_SITE_COUNT,ack,&f,f.cutoff));
        assert(f.acks==1 && f.owner.phase==AP_GROUPED_UNKNOWN);
        if(action==1)assert(!f.owner.attempted_sites && !f.io.writes_count);
        else ledger(&f);
        assert(ap_grouped_io_activate(&f.io,&f.owner,deadline()));done(&f);
    }
}
int main(void) {
    full_adoption();every_site_history_refusals();admission_refusals();ack_and_late_failure_custody();
    puts("complete adoption:1 exact17 success,510 per-site history refusals,7 admission refusals,4 ACK/late-failure custody controls; regular files only; no native authority");
    return 0;
}
