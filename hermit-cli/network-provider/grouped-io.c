/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include "grouped-io.h"
#include <errno.h>
#include <fcntl.h>
#include <linux/magic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/statfs.h>
#include <time.h>
#include <unistd.h>
static int invalid(void) {errno=EINVAL;return -1;}
static int refuse(void) {errno=ENODATA;return -1;}
static int now_ns(uint64_t *out) {
    struct timespec t;
    if(clock_gettime(CLOCK_MONOTONIC,&t))return -1;
    if(t.tv_sec<0 || (uint64_t)t.tv_sec>UINT64_MAX/1000000000ULL || t.tv_nsec<0 || t.tv_nsec>=1000000000)return refuse();
    *out=(uint64_t)t.tv_sec*1000000000ULL+(uint64_t)t.tv_nsec;return 0;
}
static int within(uint64_t deadline) {
    uint64_t now;if(now_ns(&now))return -1;
    if(!deadline || now>=deadline) {errno=ETIMEDOUT;return -1;}
    return 0;
}
static int duplicate(struct ap_grouped_io *io,unsigned role,int source) {
    struct stat st;struct statfs fs;
    if(role>=AP_GROUP_FDS || source<0 || io->fd[role]!=-1)return invalid();
    int flags=fcntl(source,F_GETFL),fdflags=fcntl(source,F_GETFD);
    if(flags<0||fdflags<0||fstat(source,&st)||fstatfs(source,&fs))return -1;
    const int expected=role==AP_GROUP_CONTROL?O_RDWR:O_RDONLY;
    /* Kernel O_LARGEFILE is 0100000 even when libc's x86_64 macro is zero. */
    if((flags&O_ACCMODE)!=expected || (flags&~(O_ACCMODE|0100000|O_DIRECTORY|O_NOFOLLOW)) ||
       st.st_uid || st.st_gid || !st.st_ino || fs.f_type!=TRACEFS_MAGIC ||
       (role==AP_GROUP_EVENTS?!S_ISDIR(st.st_mode):!S_ISREG(st.st_mode)))return refuse();
    int owned=fcntl(source,F_DUPFD_CLOEXEC,3);if(owned<0)return -1;
    io->fd[role]=owned;io->identity[role]=st;
    struct stat actual;
    if(fstat(owned,&actual)||actual.st_dev!=st.st_dev||actual.st_ino!=st.st_ino||actual.st_mode!=st.st_mode||
       fcntl(owned,F_GETFD)!=FD_CLOEXEC)return refuse();
    return 0;
}
int ap_grouped_io_init(struct ap_grouped_io *io,const int source[3],ap_grouped_journal journal,void *context) {
    if(!io||!source||!journal)return invalid();
    memset(io,0,sizeof(*io));for(unsigned i=0;i<AP_GROUP_FDS;i++)io->fd[i]=-1;
    io->journal=journal;io->journal_context=context;
    io->buffer=malloc(AP_GROUPED_CENSUS_BYTES+1);if(!io->buffer)return -1;
    io->proof=malloc(AP_GROUPED_CENSUS_BYTES+1);if(!io->proof)return -1;
    for(unsigned i=0;i<3;i++)if(duplicate(io,i,source[i]))return -1;
    return 0;
}
int ap_grouped_io_join(struct ap_grouped_io *io,const int source[3]) {
    if(!io||!io->buffer||!source)return invalid();
    for(unsigned i=0;i<3;i++)if(io->fd[i]<0 || io->fd[AP_GROUP_ID+i]>=0)return invalid();
    for(unsigned i=0;i<3;i++)if(duplicate(io,AP_GROUP_ID+i,source[i]))return -1;
    /* Eventfs can give different simultaneously held leaves the same inode.
     * Role authority is the authenticated manager join plus exact contents;
     * never require invented per-leaf inode uniqueness. */
    return 0;
}
int ap_grouped_io_release(struct ap_grouped_io *io) {
    if(!io)return invalid();
    int failed=0,saved=0;
    for(unsigned i=AP_GROUP_FDS;i;i--)if(io->fd[i-1]>=0) {
        int fd=io->fd[i-1];io->fd[i-1]=-1;
        /* Never retry an ambiguous close on a now reusable descriptor. */
        if(close(fd)&&!failed) {failed=-1;saved=errno;}
    }
    free(io->proof);io->proof=NULL;free(io->buffer);io->buffer=NULL;
    if(failed)errno=saved;
    return failed;
}
static ssize_t read_complete(struct ap_grouped_io *io,unsigned role,uint64_t deadline) {
    if(!io||!io->buffer||role>=AP_GROUP_FDS||io->fd[role]<0)return invalid();
    if(within(deadline)||lseek(io->fd[role],0,SEEK_SET))return -1;
    size_t n=0;
    for(;;) {
        if(within(deadline))return -1;
        ssize_t got=read(io->fd[role],io->buffer+n,AP_GROUPED_CENSUS_BYTES+1-n);
        if(got<0)return -1;
        if(!got)break;
        n+=(size_t)got;
        if(n>AP_GROUPED_CENSUS_BYTES) {errno=EOVERFLOW;return -1;}
    }
    if(within(deadline))return -1;
    return (ssize_t)n;
}
static int current_census(struct ap_grouped_io *io,const struct ap_grouped_owner *s,uint64_t deadline,uint32_t *mask) {
    ssize_t n=read_complete(io,AP_GROUP_CONTROL,deadline);
    return n<0?-1:ap_grouped_census(s,io->buffer,(size_t)n,mask);
}
static struct ap_grouped_write *journal_write(struct ap_grouped_io *io,const struct ap_grouped_owner *s,
        unsigned role,unsigned remove,const char *line,size_t bytes,uint64_t deadline) {
    if(!io->journal || io->writes_count>=AP_GROUPED_SITE_COUNT*2 || within(deadline)) {errno=ENODATA;return NULL;}
    struct ap_grouped_write *receipt=&io->writes[io->writes_count++];
    *receipt=(struct ap_grouped_write){.role=role,.remove=remove,.submitted=bytes};
    if(io->journal(io->journal_context,s,receipt,line)||within(deadline))return NULL;
    receipt->started=1;
    receipt->raw=write(io->fd[AP_GROUP_CONTROL],line,bytes);receipt->error=receipt->raw<0?errno:0;
    receipt->completed=1;
    if(io->journal(io->journal_context,s,receipt,line)||within(deadline))return NULL;
    return receipt;
}
int ap_grouped_io_create(struct ap_grouped_io *io,struct ap_grouped_owner *s,uint64_t deadline) {
    if(!io||!s||s->phase!=AP_GROUPED_ACKED)return invalid();
    for(unsigned role=1;role<=AP_GROUPED_SITE_COUNT;role++) {
        ssize_t n=read_complete(io,AP_GROUP_CONTROL,deadline);if(n<0)return -1;
        char line[AP_GROUPED_LINE_BYTES];int bytes=ap_grouped_create_begin(s,io->buffer,(size_t)n,role,line,sizeof(line));
        if(bytes<0||within(deadline))return -1;
        struct ap_grouped_write *receipt=journal_write(io,s,role,0,line,(size_t)bytes,deadline);
        if(!receipt) {s->phase=AP_GROUPED_UNKNOWN;s->write_unknown=1;return -1;}
        n=read_complete(io,AP_GROUP_CONTROL,deadline);
        if(n<0) {s->phase=AP_GROUPED_UNKNOWN;s->write_unknown=1;return -1;}
        int rc=ap_grouped_create_observed(s,role,receipt->raw,(size_t)bytes,io->buffer,(size_t)n);
        if(rc) {if(receipt->raw<0)errno=receipt->error;return -1;}
    }
    return within(deadline);
}
/* Compare all semantic fields explicitly: C padding is not wire authority. */
static int same_owner(const struct ap_grouped_owner *a,const struct ap_grouped_owner *b) {
    return a->incarnation==b->incarnation && !memcmp(a->group,b->group,sizeof(a->group)) &&
        !memcmp(a->event,b->event,sizeof(a->event)) && a->phase==b->phase &&
        a->verified_sites==b->verified_sites && a->attempted_sites==b->attempted_sites &&
        a->event_id==b->event_id && a->write_unknown==b->write_unknown &&
        a->pending_role==b->pending_role && a->pending_remove==b->pending_remove &&
        a->pending_bytes==b->pending_bytes;
}
static int creation_write(const struct ap_grouped_write *w,unsigned role,size_t bytes,int completed) {
    return w->role==role && !w->remove && w->submitted==bytes && !w->error &&
        w->started==completed && w->completed==completed &&
        w->raw==(completed?(ssize_t)bytes:0);
}
int ap_grouped_io_adopt_created(struct ap_grouped_io *io,struct ap_grouped_owner *s,
        const struct ap_grouped_creation_pair *pairs,size_t count,
        ap_grouped_adoption_ack acknowledge,void *context,uint64_t deadline) {
    if(!io||!s||!pairs||!acknowledge||count!=AP_GROUPED_SITE_COUNT ||
       !io->buffer||!io->proof||!io->journal||io->writes_count||s->phase!=AP_GROUPED_EMPTY)
        return invalid();
    for(unsigned i=0;i<3;i++)if(io->fd[i]<0 || io->fd[AP_GROUP_ID+i]>=0)return invalid();
    struct ap_grouped_owner expected;
    if(ap_grouped_owner_init(&expected,s->incarnation,s->group+7,AP_GROUPED_NONCE_BYTES) ||
       !same_owner(s,&expected) || within(deadline))goto failed;
    if(ap_grouped_owner_ack(&expected,s->incarnation,s->group+7,AP_GROUPED_NONCE_BYTES))goto failed;
    size_t census_bytes=0;
    for(unsigned i=0;i<AP_GROUPED_SITE_COUNT;i++) {
        const unsigned role=i+1;const struct ap_grouped_creation_pair *pair=&pairs[i];
        char line[AP_GROUPED_LINE_BYTES];
        int bytes=ap_grouped_create_begin(&expected,io->proof,census_bytes,role,line,sizeof(line));
        if(bytes<=0 || within(deadline) ||
           !same_owner(&expected,&pair->intent_owner) || !same_owner(&expected,&pair->outcome_owner) ||
           !creation_write(&pair->intent,role,(size_t)bytes,0) ||
           !creation_write(&pair->outcome,role,(size_t)bytes,1) ||
           memcmp(pair->line,line,(size_t)bytes) || pair->line[bytes] ||
           (size_t)bytes>AP_GROUPED_CENSUS_BYTES-census_bytes)goto failed;
        for(size_t j=(size_t)bytes+1;j<sizeof(pair->line);j++)if(pair->line[j])goto failed;
        memcpy(io->proof+census_bytes,line,(size_t)bytes);census_bytes+=(size_t)bytes;
        if(ap_grouped_create_observed(&expected,role,bytes,(size_t)bytes,io->proof,census_bytes))goto failed;
    }
    if(expected.phase!=AP_GROUPED_CREATED || expected.verified_sites!=AP_GROUPED_ALL_SITES ||
       expected.attempted_sites!=AP_GROUPED_ALL_SITES || expected.pending_role || expected.write_unknown)
        goto failed;
    uint32_t mask;
    if(current_census(io,&expected,deadline,&mask) || mask!=AP_GROUPED_ALL_SITES)goto failed;
    ssize_t profile_bytes=read_complete(io,AP_GROUP_PROFILE,deadline);
    if(profile_bytes<0 || ap_grouped_profile(&expected,io->buffer,(size_t)profile_bytes,AP_GROUPED_SITE_COUNT) ||
       within(deadline) || acknowledge(context,&expected,io->fd,pairs,count))goto failed;
    /* A positive keeper ACK now grants these actual retained descriptions and
     * exact history. Preserve them BEFORE any later read/deadline can fail.
     * UNKNOWN remains recoverable by the retained owner, never activatable. */
    *s=expected;s->phase=AP_GROUPED_UNKNOWN;
    for(unsigned i=0;i<AP_GROUPED_SITE_COUNT;i++)io->writes[i]=pairs[i].outcome;
    io->writes_count=AP_GROUPED_SITE_COUNT;
    if(within(deadline) || current_census(io,&expected,deadline,&mask) || mask!=AP_GROUPED_ALL_SITES)
        goto failed;
    profile_bytes=read_complete(io,AP_GROUP_PROFILE,deadline);
    if(profile_bytes<0 || ap_grouped_profile(&expected,io->buffer,(size_t)profile_bytes,AP_GROUPED_SITE_COUNT) ||
       within(deadline))goto failed;
    s->phase=AP_GROUPED_CREATED;return 0;
failed:
    s->phase=AP_GROUPED_UNKNOWN;return refuse();
}
/* Validate actual libc write shape without claiming what reached the kernel.
 * A short, failed, or absent final outcome remains an attempted prefix only. */
static int recovery_outcome(const struct ap_grouped_write *w,unsigned role,size_t bytes) {
    if(w->role!=role || w->remove || w->submitted!=bytes ||
       w->started!=1 || w->completed!=1)return 0;
    return (w->raw==-1 && w->error>0 && w->error<=4095) ||
        (w->raw>=0 && (size_t)w->raw<=bytes && !w->error);
}
static int empty_recovery_outcome(const struct ap_grouped_creation_pair *pair) {
    const struct ap_grouped_owner empty={0};
    const struct ap_grouped_write *w=&pair->outcome;
    return same_owner(&pair->outcome_owner,&empty) && !w->role && !w->remove &&
        !w->submitted && !w->raw && !w->error && !w->started && !w->completed;
}
int ap_grouped_io_adopt_recovery(struct ap_grouped_io *io,struct ap_grouped_owner *s,
        const struct ap_grouped_recovery_step *steps,size_t count,
        ap_grouped_recovery_ack acknowledge,void *context,uint64_t deadline) {
    if(!io||!s||!steps||!acknowledge||!count||count>AP_GROUPED_SITE_COUNT ||
       !io->buffer||!io->proof||!io->journal||io->writes_count||s->phase!=AP_GROUPED_EMPTY)
        return invalid();
    for(unsigned i=0;i<3;i++)if(io->fd[i]<0 || io->fd[AP_GROUP_ID+i]>=0)return invalid();
    struct ap_grouped_owner expected;
    if(ap_grouped_owner_init(&expected,s->incarnation,s->group+7,AP_GROUPED_NONCE_BYTES) ||
       !same_owner(s,&expected) || within(deadline))goto failed;
    if(ap_grouped_owner_ack(&expected,s->incarnation,s->group+7,AP_GROUPED_NONCE_BYTES))goto failed;
    size_t census_bytes=0;
    for(size_t i=0;i<count;i++) {
        const unsigned role=(unsigned)i+1;
        const struct ap_grouped_recovery_step *step=&steps[i];
        const struct ap_grouped_creation_pair *pair=&step->pair;
        char line[AP_GROUPED_LINE_BYTES];
        int bytes=ap_grouped_create_begin(&expected,io->proof,census_bytes,role,line,sizeof(line));
        if(bytes<=0 || within(deadline) || !same_owner(&expected,&pair->intent_owner) ||
           !creation_write(&pair->intent,role,(size_t)bytes,0) ||
           memcmp(pair->line,line,(size_t)bytes) || pair->line[bytes] ||
           step->outcome_present>1 || (size_t)bytes>AP_GROUPED_CENSUS_BYTES-census_bytes)
            goto failed;
        for(size_t j=(size_t)bytes+1;j<sizeof(pair->line);j++)if(pair->line[j])goto failed;
        if(!step->outcome_present) {
            if(i+1!=count || !empty_recovery_outcome(pair))goto failed;
            expected.write_unknown=1;
            break;
        }
        if(!same_owner(&expected,&pair->outcome_owner) ||
           !recovery_outcome(&pair->outcome,role,(size_t)bytes))goto failed;
        if(pair->outcome.raw!=(ssize_t)bytes) {
            if(i+1!=count)goto failed;
            expected.write_unknown=1;
            break;
        }
        memcpy(io->proof+census_bytes,line,(size_t)bytes);census_bytes+=(size_t)bytes;
        if(ap_grouped_create_observed(&expected,role,bytes,(size_t)bytes,io->proof,census_bytes))goto failed;
    }
    /* No activation state is ever exposed, even for a complete successful
     * transcript. The actual namespace may contain any subset of our attempts. */
    expected.phase=AP_GROUPED_UNKNOWN;
    uint32_t mask;
    if(!expected.attempted_sites || current_census(io,&expected,deadline,&mask) ||
       (mask&~expected.attempted_sites))goto failed;
    ssize_t profile_bytes=read_complete(io,AP_GROUP_PROFILE,deadline);
    if(profile_bytes<0 || ap_grouped_profile(&expected,io->buffer,(size_t)profile_bytes,
        (unsigned)__builtin_popcount(mask)) || within(deadline) ||
       acknowledge(context,&expected,io->fd,steps,count))goto failed;
    /* Retain exact attempted custody immediately after ACK, before any next
     * clock/census read can fail. Unknown writes do not become successful. */
    *s=expected;
    for(size_t i=0;i<count;i++)io->writes[i]=steps[i].outcome_present?
        steps[i].pair.outcome:steps[i].pair.intent;
    io->writes_count=(unsigned)count;
    if(within(deadline))goto failed;
    ssize_t definition_bytes=read_complete(io,AP_GROUP_CONTROL,deadline);
    if(definition_bytes<0 || ap_grouped_census(s,io->buffer,(size_t)definition_bytes,&mask) ||
       (mask&~s->attempted_sites))goto failed;
    memcpy(io->proof,io->buffer,(size_t)definition_bytes);
    profile_bytes=read_complete(io,AP_GROUP_PROFILE,deadline);
    if(profile_bytes<0 || ap_grouped_profile(s,io->buffer,(size_t)profile_bytes,
        (unsigned)__builtin_popcount(mask)) || within(deadline) ||
       ap_grouped_recover(s,io->proof,(size_t)definition_bytes))goto failed;
    return 0;
failed:
    s->phase=AP_GROUPED_UNKNOWN;return refuse();
}
int ap_grouped_io_bind(struct ap_grouped_io *io,struct ap_grouped_owner *s,uint64_t deadline) {
    if(!io||!s||s->phase!=AP_GROUPED_CREATED)return invalid();
    char id[16],format[4096],enable[2];ssize_t a=read_complete(io,AP_GROUP_ID,deadline);
    if(a<0||a>(ssize_t)sizeof(id))return refuse();
    memcpy(id,io->buffer,(size_t)a);
    ssize_t b=read_complete(io,AP_GROUP_FORMAT,deadline);
    if(b<0||b>(ssize_t)sizeof(format))return refuse();
    memcpy(format,io->buffer,(size_t)b);
    ssize_t c=read_complete(io,AP_GROUP_ENABLE,deadline);
    if(c<0||c>(ssize_t)sizeof(enable))return refuse();
    memcpy(enable,io->buffer,(size_t)c);
    return ap_grouped_bind_leaves(s,id,(size_t)a,format,(size_t)b,enable,(size_t)c);
}
int ap_grouped_io_health(struct ap_grouped_io *io,const struct ap_grouped_owner *s,uint64_t deadline) {
    uint32_t mask;
    if(!s||(s->phase!=AP_GROUPED_LEAVES&&s->phase!=AP_GROUPED_ACTIVE))return invalid();
    if(current_census(io,s,deadline,&mask)||mask!=AP_GROUPED_ALL_SITES)return refuse();
    ssize_t n=read_complete(io,AP_GROUP_PROFILE,deadline);
    if(n<0||ap_grouped_profile(s,io->buffer,(size_t)n,AP_GROUPED_SITE_COUNT))return -1;
    n=read_complete(io,AP_GROUP_ENABLE,deadline);
    return n==2&&!memcmp(io->buffer,"0\n",2)?0:refuse();
}
int ap_grouped_io_activate(struct ap_grouped_io *io,struct ap_grouped_owner *s,uint64_t deadline) {
    if(ap_grouped_io_health(io,s,deadline))return -1;
    ssize_t n=read_complete(io,AP_GROUP_CONTROL,deadline);if(n<0)return -1;
    size_t bytes=(size_t)n;memcpy(io->proof,io->buffer,bytes);
    ssize_t pn=read_complete(io,AP_GROUP_PROFILE,deadline);if(pn<0)return -1;
    return ap_grouped_activate(s,io->proof,bytes,io->buffer,(size_t)pn);
}
static int directory_absent(struct ap_grouped_io *io,const struct ap_grouped_owner *s) {
    char path[AP_GROUPED_NAME_BYTES*2];int n=snprintf(path,sizeof(path),"%s/%s",s->group,s->event);
    if(n<0||(size_t)n>=sizeof(path))return invalid();
    struct stat st;
    if(!fstatat(io->fd[AP_GROUP_EVENTS],path,&st,AT_SYMLINK_NOFOLLOW)||errno!=ENOENT)return refuse();
    if(!fstatat(io->fd[AP_GROUP_EVENTS],s->group,&st,AT_SYMLINK_NOFOLLOW)||errno!=ENOENT)return refuse();
    return 0;
}
int ap_grouped_io_recover_terminal(struct ap_grouped_io *io,struct ap_grouped_owner *s,
        uint64_t release_start,uint64_t enclosing_cutoff) {
    uint64_t now;if(now_ns(&now))return -1;
    if(!io||!s||!release_start||release_start>now||release_start>UINT64_MAX-1000000000ULL||
       s->attempted_sites!=AP_GROUPED_ALL_SITES||
       (s->phase!=AP_GROUPED_LEAVES&&s->phase!=AP_GROUPED_UNKNOWN))return invalid();
    const uint64_t original=release_start+1000000000ULL;
    const uint64_t deadline=original<enclosing_cutoff?original:enclosing_cutoff;
    if(within(deadline))return -1;
    ssize_t n=read_complete(io,AP_GROUP_CONTROL,deadline);
    if(n<0||ap_grouped_recover(s,io->buffer,(size_t)n))return -1;
    return within(deadline);
}
/* One body for both APIs: every original read, callback, write, comparator,
 * order and deadline check below is preserved. Only the supplied cutoff may
 * be earlier; the original release timestamp is never resampled. */
static int delete_before(struct ap_grouped_io *io,struct ap_grouped_owner *s,uint64_t deadline) {
    if(within(deadline)||(s->phase!=AP_GROUPED_QUIESCENT&&s->phase!=AP_GROUPED_CLEANING))return -1;
    for(unsigned role=AP_GROUPED_SITE_COUNT;role;role--)if(s->verified_sites&(1U<<(role-1))) {
        ssize_t n=read_complete(io,AP_GROUP_CONTROL,deadline);if(n<0)return -1;
        char line[AP_GROUPED_LINE_BYTES];int bytes=ap_grouped_delete_begin(s,io->buffer,(size_t)n,role,line,sizeof(line));
        if(bytes<0||within(deadline))return -1;
        struct ap_grouped_write *receipt=journal_write(io,s,role,1,line,(size_t)bytes,deadline);
        if(!receipt) {s->phase=AP_GROUPED_UNKNOWN;s->write_unknown=1;return -1;}
        n=read_complete(io,AP_GROUP_CONTROL,deadline);
        if(n<0) {s->phase=AP_GROUPED_UNKNOWN;s->write_unknown=1;return -1;}
        int rc=ap_grouped_delete_observed(s,role,receipt->raw,(size_t)bytes,io->buffer,(size_t)n);
        if(rc) {if(receipt->raw<0)errno=receipt->error;return -1;}
    }
    uint32_t mask;if(current_census(io,s,deadline,&mask)||mask||directory_absent(io,s))return -1;
    ssize_t n=read_complete(io,AP_GROUP_PROFILE,deadline);
    if(n<0||ap_grouped_profile(s,io->buffer,(size_t)n,0))return -1;
    size_t profile_bytes=(size_t)n;memcpy(io->proof,io->buffer,profile_bytes);
    /* Re-read the full definition projection after profile and directory
     * queries. Absence is never inferred from successful writes alone. */
    n=read_complete(io,AP_GROUP_CONTROL,deadline);
    if(n<0||ap_grouped_census(s,io->buffer,(size_t)n,&mask)||mask||directory_absent(io,s)||within(deadline))return -1;
    return ap_grouped_absent(s,io->buffer,(size_t)n,io->proof,profile_bytes,1);
}

int ap_grouped_io_delete_until(struct ap_grouped_io *io,struct ap_grouped_owner *s,
        uint64_t release_start,uint64_t enclosing_cutoff) {
    uint64_t now;if(now_ns(&now))return -1;
    if(!io||!s||!release_start||release_start>now||release_start>UINT64_MAX-1000000000ULL)return invalid();
    const uint64_t original=release_start+1000000000ULL;
    const uint64_t deadline=original<enclosing_cutoff?original:enclosing_cutoff;
    return delete_before(io,s,deadline);
}
int ap_grouped_io_delete(struct ap_grouped_io *io,struct ap_grouped_owner *s,uint64_t release_start) {
    return ap_grouped_io_delete_until(io,s,release_start,UINT64_MAX);
}
/* These are results of complete-read helpers, not invented individual read(2)
 * returns. The returned marker is set only after the actual helper returns. */
static ssize_t observe_read(struct ap_grouped_io *io,unsigned role,uint64_t cutoff,
        struct ap_grouped_complete_read_observation *out) {
    out->attempted=1;
    ssize_t result=read_complete(io,role,cutoff);const int error=result<0?errno:0;
    out->result=result;out->error=error;out->returned=1;
    if(result<0)errno=error;
    return result;
}
static int observe_directory(struct ap_grouped_io *io,const char *path,uint64_t cutoff,
        struct ap_grouped_directory_observation *out) {
    if(within(cutoff))return -1;
    struct stat st;out->attempted=1;
    int result=fstatat(io->fd[AP_GROUP_EVENTS],path,&st,AT_SYMLINK_NOFOLLOW);
    const int error=result<0?errno:0;
    out->result=result;out->error=error;out->returned=1;
    if(result!=-1||error!=ENOENT)return refuse();
    return within(cutoff);
}
int ap_grouped_io_observe_absent(struct ap_grouped_io *io,const struct ap_grouped_owner *s,
        uint64_t cutoff,struct ap_grouped_absence_observation *out) {
    if(!out)return invalid();
    *out=(struct ap_grouped_absence_observation){.cutoff=cutoff};
    if(!io||!s||!io->buffer||!io->proof||s->phase!=AP_GROUPED_ABSENT||!s->attempted_sites||
       (s->attempted_sites&~AP_GROUPED_ALL_SITES)||s->verified_sites||s->pending_role||
       s->pending_remove||s->pending_bytes)return invalid();
    for(unsigned i=0;i<3;i++)if(io->fd[i]<0)return invalid();
    if(now_ns(&out->started_ns))return -1;
    out->started_observed=1;
    if(!cutoff||out->started_ns>=cutoff){errno=ETIMEDOUT;return -1;}
    uint32_t mask;
    ssize_t n=observe_read(io,AP_GROUP_CONTROL,cutoff,&out->definitions);
    if(n<0)return -1;
    if(ap_grouped_census(s,io->buffer,(size_t)n,&mask)||mask)return refuse();
    out->definition_bytes=(size_t)n;memcpy(io->proof,io->buffer,(size_t)n);
    n=observe_read(io,AP_GROUP_PROFILE,cutoff,&out->profile);
    if(n<0)return -1;
    if(ap_grouped_profile(s,io->buffer,(size_t)n,0))return -1;
    out->profile_bytes=(size_t)n;
    char path[AP_GROUPED_NAME_BYTES*2];int length=snprintf(path,sizeof(path),"%s/%s",s->group,s->event);
    if(length<0||(size_t)length>=sizeof(path))return invalid();
    if(observe_directory(io,path,cutoff,&out->event_directory)||
       observe_directory(io,s->group,cutoff,&out->group_directory))return -1;
    if(now_ns(&out->finished_ns))return -1;
    out->finished_observed=1;
    if(out->finished_ns<out->started_ns||out->finished_ns>=cutoff){errno=ETIMEDOUT;return -1;}
    out->complete=1;return 0;
}
