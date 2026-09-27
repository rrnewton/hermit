/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include "grouped-broker-bridge.h"
#include "grouped-adoption-wire.h"
#include "grouped-guardian-bootstrap.h"
#include "grouped-keeper-dual.h"
#include "grouped-api.h"
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

struct hermit_grouped_broker {
    struct ap_grouped_owner owner;
    struct ap_grouped_io io;
    struct grouped_keeper_wire keeper;
    struct grouped_guardian_bootstrap bootstrap;
    struct grouped_keeper_dual dual;
    struct grouped_adoption_wire adoption;
    struct hermit_grouped_broker_status status;
    hermit_grouped_digest digest;
    struct hermit_grouped_runtime_handoff runtime;
    unsigned provider_lease_issued,unopened_retirement_attempted;
};
/* Only the unchanged adoption codec's SHA256 symbol is renamed at compile
 * time. The callback is scoped to one synchronous entry and one actual thread;
 * neither an environment library nor process-global mutable callback is used. */
static _Thread_local struct hermit_grouped_broker *digest_owner;
unsigned char *hermit_grouped_broker_sha256(const unsigned char *bytes,size_t count,unsigned char *out) {
    struct hermit_grouped_broker *b=digest_owner;
    if(!b||!b->digest||!bytes||!out||count>GA_TRANSFER_BYTES) {errno=EINVAL;return NULL;}
    if(b->digest(bytes,count,out)) {errno=EIO;return NULL;}
    return out;
}
static int fail(struct hermit_grouped_broker *b,int error) {
    if(b) {b->status.refused=1;if(!b->status.error)b->status.error=error?error:EIO;errno=(int)b->status.error;}
    else errno=EINVAL;
    return -1;
}
unsigned hermit_grouped_broker_abi(void) {return HERMIT_GROUPED_BROKER_ABI;}
int hermit_grouped_broker_alloc(struct hermit_grouped_broker **out) {
    if(!out||*out) {errno=EINVAL;return -1;}
    struct hermit_grouped_broker *b=calloc(1,sizeof(*b));if(!b)return -1;
    for(unsigned i=0;i<AP_GROUP_FDS;i++)b->io.fd[i]=-1;
    for(unsigned i=0;i<3;i++)b->adoption.controls[i]=-1;
    for(unsigned i=0;i<9;i++)b->runtime.fd[i]=-1;
    b->keeper.channel=b->keeper.keeper_pidfd=-1;
    b->bootstrap.guardian.channel=b->bootstrap.guardian.keeper_pidfd=-1;
    b->bootstrap.creator_pidfd=b->bootstrap.cgroup_directory=-1;
    b->status.abi=HERMIT_GROUPED_BROKER_ABI;*out=b;return 0;
}
static int start(struct hermit_grouped_broker *b,int channel,uint64_t incarnation,
        const char *nonce,uint64_t deadline,uint64_t cutoff,const char *unit) {
    if(!b||b->status.attempted||b->status.refused||b->status.aliases_released)return fail(b,EINVAL);
    b->status.attempted=1;b->status.incarnation=incarnation;b->status.deadline=deadline;b->status.creator_cutoff=cutoff;
    int owned=fcntl(channel,F_DUPFD_CLOEXEC,3);if(owned<0)return fail(b,errno);
    b->keeper.channel=owned;
    if(grouped_keeper_wire_init(&b->keeper,owned,incarnation,nonce,deadline)||
       grouped_guardian_bootstrap_init(&b->bootstrap,&b->keeper,cutoff)||
       grouped_guardian_bootstrap_creator(&b->bootstrap,unit))return fail(b,errno);
    return 0;
}
static int controls(struct hermit_grouped_broker *b,const int fds[3]) {
    if(grouped_guardian_bootstrap_controls(&b->bootstrap,fds)||
       grouped_keeper_dual_init(&b->dual,&b->bootstrap.guardian,&b->keeper)||
       ap_grouped_io_init(&b->io,fds,grouped_keeper_dual_journal,&b->dual)||
       ap_grouped_owner_init(&b->owner,b->keeper.incarnation,b->keeper.nonce,AP_GROUPED_NONCE_BYTES))return fail(b,errno);
    return 0;
}
int hermit_grouped_broker_source(struct hermit_grouped_broker *b,int channel,uint64_t incarnation,
        const char *nonce,uint64_t deadline,uint64_t cutoff,const char *unit,const int fds[3]) {
    if(start(b,channel,incarnation,nonce,deadline,cutoff,unit)||controls(b,fds)||
       ap_grouped_owner_ack(&b->owner,incarnation,nonce,AP_GROUPED_NONCE_BYTES))return fail(b,errno);
    b->status.source_ready=1;return 0;
}
int hermit_grouped_broker_create(struct hermit_grouped_broker *b) {
    if(!b||b->status.refused||!b->status.source_ready||b->status.source_created||b->status.aliases_released)return fail(b,EINVAL);
    if(ap_grouped_io_create(&b->io,&b->owner,b->status.deadline))return fail(b,errno);
    b->status.source_created=1;return 0;
}
int hermit_grouped_broker_successor(struct hermit_grouped_broker *b,int channel,uint64_t incarnation,
        const char *nonce,uint64_t deadline,uint64_t cutoff,const char *unit,const int leaves[3],hermit_grouped_digest digest) {
    if(!digest||digest_owner)return fail(b,EINVAL);
    if(start(b,channel,incarnation,nonce,deadline,cutoff,unit))return -1;
    b->digest=digest;
    if(grouped_adoption_wire_init(&b->adoption,&b->keeper))return fail(b,errno);
    digest_owner=b;int received=grouped_adoption_wire_receive(&b->adoption);int error=errno;digest_owner=NULL;
    if(received)return fail(b,error);
    if(controls(b,b->adoption.controls)||
       ap_grouped_io_adopt_created(&b->io,&b->owner,b->adoption.pairs,AP_GROUPED_SITE_COUNT,
           grouped_adoption_wire_ack,&b->adoption,deadline))return fail(b,errno);
    b->status.successor_adopted=1;
    if(ap_grouped_io_join(&b->io,leaves)||ap_grouped_io_bind(&b->io,&b->owner,deadline))return fail(b,errno);
    b->status.leaves_bound=1;return 0;
}
static void runtime_close(struct hermit_grouped_broker *b,unsigned row,int *slot,int *first) {
    if(*slot<0)return;
    int fd=*slot;*slot=-1;b->runtime.fd[row]=fd;b->runtime.attempted|=1U<<row;
    int raw=close(fd),error=raw?errno:0;
    b->runtime.raw[row]=raw;b->runtime.error[row]=error;b->runtime.returned|=1U<<row;
    if(raw&&!*first)*first=error;
}
int hermit_grouped_broker_runtime_controls(struct hermit_grouped_broker *b,int out[3]) {
    if(!b||!out||b->status.refused||b->status.aliases_released||b->provider_lease_issued||
       b->unopened_retirement_attempted||b->runtime.installed||!b->status.successor_adopted||
       !b->status.leaves_bound||b->owner.phase!=AP_GROUPED_LEAVES)return fail(b,EINVAL);
    for(unsigned i=0;i<3;i++)if(b->io.fd[i]<0)return fail(b,EINVAL);
    for(unsigned i=0;i<3;i++)out[i]=b->io.fd[i];
    return 0;
}
int hermit_grouped_broker_runtime_journal(struct hermit_grouped_broker *b,
        hermit_grouped_runtime_journal journal,void *context) {
    if(!b||!journal||!context||b->runtime.installed||b->runtime.aliases_released||
       b->status.refused||b->status.aliases_released||!b->status.successor_adopted||
       !b->status.leaves_bound||b->owner.phase!=AP_GROUPED_LEAVES||
       b->owner.attempted_sites!=AP_GROUPED_ALL_SITES||b->owner.verified_sites!=AP_GROUPED_ALL_SITES||
       b->owner.write_unknown||b->owner.pending_role||b->owner.pending_remove||b->owner.pending_bytes)
        return fail(b,EINVAL);
    for(unsigned i=0;i<AP_GROUP_FDS;i++)if(b->io.fd[i]<0)return fail(b,EINVAL);
    for(unsigned i=0;i<3;i++)if(b->adoption.controls[i]<0)return fail(b,EINVAL);
    if(b->bootstrap.cgroup_directory<0||b->bootstrap.creator_pidfd<0||
       b->bootstrap.guardian.keeper_pidfd<0||b->bootstrap.guardian.channel<0||
       b->keeper.keeper_pidfd<0||b->keeper.channel<0)return fail(b,EINVAL);
    /* The retained actual runtime owner precedes this irreversible pointer
     * transfer. Original buffers, control/leaf aliases and owner stay in place. */
    b->runtime.installed=1;b->io.journal=journal;b->io.journal_context=context;
    b->runtime.aliases_released=1;int error=0;
    for(unsigned i=0;i<3;i++)runtime_close(b,i,&b->adoption.controls[i],&error);
    runtime_close(b,3,&b->bootstrap.cgroup_directory,&error);
    runtime_close(b,4,&b->bootstrap.creator_pidfd,&error);
    runtime_close(b,5,&b->bootstrap.guardian.keeper_pidfd,&error);
    runtime_close(b,6,&b->bootstrap.guardian.channel,&error);
    runtime_close(b,7,&b->keeper.keeper_pidfd,&error);
    runtime_close(b,8,&b->keeper.channel,&error);
    return error?fail(b,error):0;
}
int hermit_grouped_broker_runtime_status(const struct hermit_grouped_broker *b,
        struct hermit_grouped_runtime_handoff *out) {
    if(!b||!out) {errno=EINVAL;return -1;}
    *out=b->runtime;return 0;
}
int hermit_grouped_broker_provider_lease(struct hermit_grouped_broker *b,uint64_t incarnation,
        struct ap_grouped_io **io,struct ap_grouped_owner **owner) {
    if(!b||!io||!owner||*io||*owner||!incarnation||b->provider_lease_issued||
       b->unopened_retirement_attempted||b->status.refused||b->status.aliases_released||
       !b->status.successor_adopted||!b->status.leaves_bound||!b->runtime.installed||
       !b->runtime.aliases_released||b->status.incarnation!=incarnation||
       b->owner.incarnation!=incarnation||b->owner.phase!=AP_GROUPED_LEAVES||
       b->owner.attempted_sites!=AP_GROUPED_ALL_SITES||b->owner.verified_sites!=AP_GROUPED_ALL_SITES||
       b->owner.write_unknown||b->owner.pending_role||b->owner.pending_remove||b->owner.pending_bytes||
       !b->io.buffer||!b->io.proof||!b->io.journal)return fail(b,EINVAL);
    for(unsigned i=0;i<AP_GROUP_FDS;i++)if(b->io.fd[i]<0)return fail(b,EINVAL);
    b->provider_lease_issued=1;*io=&b->io;*owner=&b->owner;return 0;
}
int hermit_grouped_broker_retire_unopened(struct hermit_grouped_broker *b,
        uint64_t release_start,uint64_t enclosing_cutoff) {
    if(!b||b->unopened_retirement_attempted||b->provider_lease_issued||b->status.refused||
       b->status.aliases_released||!b->status.successor_adopted||!b->status.leaves_bound||
       !b->runtime.installed||!b->runtime.aliases_released||b->owner.phase!=AP_GROUPED_LEAVES||
       b->owner.attempted_sites!=AP_GROUPED_ALL_SITES||b->owner.verified_sites!=AP_GROUPED_ALL_SITES)
        return fail(b,EINVAL);
    b->unopened_retirement_attempted=1;
    /* No pointer lease has ever left this actual context. The private caller
     * additionally holds original controller terminal/live peer/cursor proof;
     * this NULL is a no-call state, never an invented BPF absence inventory. */
    struct ap_session *session=NULL;
    int raw=ap_close_grouped_startup_terminal(&session,&b->io,&b->owner,release_start,enclosing_cutoff);
    if(raw)return fail(b,errno);
    if(session)return fail(b,EPROTO);
    return 0;
}
int hermit_grouped_broker_status(const struct hermit_grouped_broker *b,struct hermit_grouped_broker_status *out) {
    if(!b||!out) {errno=EINVAL;return -1;}
    *out=b->status;out->owner_phase=(uint32_t)b->owner.phase;
    out->attempted_sites=b->owner.attempted_sites;out->verified_sites=b->owner.verified_sites;return 0;
}
static void close_owned(int *slot,int *error) {
    if(*slot<0)return;
    int fd=*slot;*slot=-1;if(close(fd)&&!*error)*error=errno;
}
int hermit_grouped_broker_release_aliases(struct hermit_grouped_broker *b) {
    if(!b||b->status.aliases_released)return fail(b,EINVAL);
    b->status.aliases_released=1;int error=0;
    if(ap_grouped_io_release(&b->io))error=errno;
    for(unsigned i=0;i<3;i++)close_owned(&b->adoption.controls[i],&error);
    close_owned(&b->bootstrap.cgroup_directory,&error);close_owned(&b->bootstrap.creator_pidfd,&error);
    close_owned(&b->bootstrap.guardian.keeper_pidfd,&error);close_owned(&b->bootstrap.guardian.channel,&error);
    close_owned(&b->keeper.keeper_pidfd,&error);close_owned(&b->keeper.channel,&error);
    return error?fail(b,error):0;
}
int hermit_grouped_broker_free(struct hermit_grouped_broker *b) {
    if(!b||!b->status.aliases_released) {errno=EINVAL;return -1;}
    free(b);return 0;
}
