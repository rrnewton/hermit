/* SPDX-License-Identifier: BSD-3-Clause */
/* Actual recovery implementation with ordinary temporary files standing in for
 * authenticated tracefs descriptions. This is not keeper/native qualification. */
#define _GNU_SOURCE
#include "grouped-io.c"
#include <assert.h>
static const char nonce[]="1234567890abcdef1234567890abcdef";
struct fixture {
    struct ap_grouped_owner owner;
    struct ap_grouped_io io;
    struct ap_grouped_recovery_step steps[AP_GROUPED_SITE_COUNT];
    FILE *control,*profile,*events;
    size_t count;
    unsigned acks,journals,action;
    uint64_t cutoff;
};
static uint32_t prefix(size_t count) {return (1U<<count)-1;}
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
        const struct ap_grouped_recovery_step *steps,size_t count) {
    struct fixture *f=opaque;f->acks++;
    assert(owner->phase==AP_GROUPED_UNKNOWN && owner->attempted_sites==prefix(f->count));
    assert(steps==f->steps && count==f->count);
    assert(fd[0]==fileno(f->control)&&fd[1]==fileno(f->profile)&&fd[2]==fileno(f->events));
    if(f->action==1)return -1;
    if(f->action==2)projection(f,1U<<16); /* fresh unattempted role after actual callback */
    if(f->action==3)replace(f->profile,"invalid\n",8);
    if(f->action==4) {
        const uint64_t wake=f->cutoff+1000000ULL;
        struct timespec until={.tv_sec=(time_t)(wake/1000000000ULL),.tv_nsec=(long)(wake%1000000000ULL)};
        assert(!clock_nanosleep(CLOCK_MONOTONIC,TIMER_ABSTIME,&until,NULL));
    }
    return 0;
}
static void init(struct fixture *f,size_t count,unsigned last) {
    *f=(struct fixture){.count=count};assert(count && count<=AP_GROUPED_SITE_COUNT && last<4);
    assert(!ap_grouped_owner_init(&f->owner,41,nonce,32));
    struct ap_grouped_owner current=f->owner;assert(!ap_grouped_owner_ack(&current,41,nonce,32));
    char history[AP_GROUPED_SITE_COUNT*AP_GROUPED_LINE_BYTES];size_t at=0;
    for(size_t i=0;i<count;i++) {
        struct ap_grouped_creation_pair *p=&f->steps[i].pair;unsigned role=(unsigned)i+1;
        int n=ap_grouped_create_begin(&current,history,at,role,p->line,sizeof(p->line));assert(n>0);
        p->intent_owner=current;p->intent=(struct ap_grouped_write){.role=role,.submitted=(size_t)n};
        if(i+1==count && last==1)break;
        f->steps[i].outcome_present=1;p->outcome_owner=current;p->outcome=p->intent;
        p->outcome.started=p->outcome.completed=1;p->outcome.raw=n;
        if(i+1==count && last==2) {p->outcome.raw=-1;p->outcome.error=EIO;break;}
        if(i+1==count && last==3) {p->outcome.raw=n-1;break;}
        memcpy(history+at,p->line,(size_t)n);at+=(size_t)n;
        assert(!ap_grouped_create_observed(&current,role,n,(size_t)n,history,at));
    }
    f->control=tmpfile();f->profile=tmpfile();f->events=tmpfile();assert(f->control&&f->profile&&f->events);
    for(unsigned i=0;i<AP_GROUP_FDS;i++)f->io.fd[i]=-1;
    f->io.fd[0]=fileno(f->control);f->io.fd[1]=fileno(f->profile);f->io.fd[2]=fileno(f->events);
    f->io.buffer=malloc(AP_GROUPED_CENSUS_BYTES+1);f->io.proof=malloc(AP_GROUPED_CENSUS_BYTES+1);
    assert(f->io.buffer&&f->io.proof);f->io.journal=journal;f->io.journal_context=f;
    projection(f,prefix(count));
}
static void done(struct fixture *f) {
    assert(!f->journals);assert(!fclose(f->control));assert(!fclose(f->profile));assert(!fclose(f->events));
    free(f->io.buffer);free(f->io.proof);
}
static uint64_t deadline(void) {uint64_t now;assert(!now_ns(&now));return now+1000000000ULL;}
static void positive_prefixes(void) {
    for(size_t count=1;count<=17;count++)for(unsigned mode=0;mode<4;mode++)for(unsigned subset=0;subset<3;subset++) {
        struct fixture f;init(&f,count,mode);
        uint32_t mask=subset==0?prefix(count):subset==1?prefix(count-1):0;projection(&f,mask);
        assert(!ap_grouped_io_adopt_recovery(&f.io,&f.owner,f.steps,count,ack,&f,deadline()));
        assert(f.acks==1 && f.owner.phase==AP_GROUPED_QUIESCENT && f.owner.attempted_sites==prefix(count));
        assert(f.owner.verified_sites==mask && !f.owner.event_id && !f.owner.pending_role && !f.owner.pending_remove && !f.owner.pending_bytes);
        assert(f.io.writes_count==count);
        for(size_t i=0;i<count;i++) {
            const struct ap_grouped_write *w=f.steps[i].outcome_present?&f.steps[i].pair.outcome:&f.steps[i].pair.intent;
            assert(!memcmp(&f.io.writes[i],w,sizeof(*w)));
        }
        assert(ap_grouped_io_bind(&f.io,&f.owner,deadline()));
        assert(ap_grouped_io_activate(&f.io,&f.owner,deadline()));
        assert(ap_grouped_io_adopt_recovery(&f.io,&f.owner,f.steps,count,ack,&f,deadline()));
        assert(f.acks==1);done(&f);
    }
}
static void malformed_refusals(void) {
    for(unsigned variant=0;variant<19;variant++) {
        struct fixture f;init(&f,3,0);struct ap_grouped_creation_pair *p=&f.steps[2].pair;
        size_t count=3;uint64_t end=deadline();
        switch(variant) {
        case 0:count=0;break;
        case 1:count=18;break;
        case 2:p->intent_owner.incarnation++;break;
        case 3:p->outcome_owner.pending_role++;break;
        case 4:p->intent.raw=1;break;
        case 5:p->outcome.remove=1;break;
        case 6:p->outcome.raw++;break;
        case 7:p->outcome.raw=-2;p->outcome.error=EIO;break;
        case 8:p->outcome.error=EIO;break;
        case 9:p->outcome.raw=-1;p->outcome.error=0;break;
        case 10:f.steps[2].outcome_present=2;break;
        case 11:f.steps[2].outcome_present=0;break; /* absent cannot conceal existing outcome */
        case 12:f.steps[0].outcome_present=0;f.steps[0].pair.outcome=(struct ap_grouped_write){0};
            f.steps[0].pair.outcome_owner=(struct ap_grouped_owner){0};break;
        case 13:f.steps[0].pair.outcome.raw=0;break; /* short non-final */
        case 14:p->line[p->intent.submitted+1]='x';break;
        case 15:p->line[0]='r';break;
        case 16:projection(&f,1U<<3);break; /* exact but unattempted fourth role */
        case 17:replace(f.profile,"invalid\n",8);break;
        case 18:end=1;break;
        default:assert(0);
        }
        assert(ap_grouped_io_adopt_recovery(&f.io,&f.owner,f.steps,count,ack,&f,end));
        assert(!f.acks && !f.owner.attempted_sites && !f.io.writes_count);
        assert(ap_grouped_recover(&f.owner,"",0));done(&f);
    }
}
static void ack_and_late_failure_custody(void) {
    for(unsigned action=1;action<=4;action++) {
        struct fixture f;init(&f,3,1);f.action=action;uint64_t end=deadline();
        f.cutoff=end;
        assert(ap_grouped_io_adopt_recovery(&f.io,&f.owner,f.steps,3,ack,&f,end));
        assert(f.acks==1 && f.owner.phase==AP_GROUPED_UNKNOWN);
        if(action==1) {assert(!f.owner.attempted_sites && !f.io.writes_count);}
        else {assert(f.owner.attempted_sites==7 && f.owner.write_unknown && f.io.writes_count==3);
            assert(f.owner.pending_role==3 && !f.io.writes[2].started && !f.io.writes[2].completed);}
        assert(ap_grouped_io_activate(&f.io,&f.owner,deadline()));done(&f);
    }
}
int main(void) {
    positive_prefixes();malformed_refusals();ack_and_late_failure_custody();
    puts("partial recovery:204 successful-prefix/subset cases,19 pre-ACK refusals,4 ACK/late-failure custody controls; regular files only; no native authority");
    return 0;
}
