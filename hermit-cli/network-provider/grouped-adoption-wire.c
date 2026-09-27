/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include "grouped-adoption-wire.h"
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/kcmp.h>
/* OpenSSL3 one-shot SHA256 ABI; use the installed runtime SONAME without
 * requiring development headers or an unversioned linker alias. */
extern unsigned char *SHA256(const unsigned char *,size_t,unsigned char *);
#define SHA256_DIGEST_LENGTH 32
#include <poll.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
static int fail(struct grouped_adoption_wire *a,int error) {
    if(a) {a->refused=1;if(!a->error)a->error=error?error:EIO;errno=a->error;}
    else errno=EINVAL;
    return -1;
}
static int hex32(const char *p,int nonzero) {
    if(!p||strnlen(p,33)!=32)return 0;
    unsigned any=0;
    for(unsigned i=0;i<32;i++) {if(!((p[i]>='0'&&p[i]<='9')||(p[i]>='a'&&p[i]<='f')))return 0;any|=p[i]!='0';}
    return !nonzero||any;
}
static int append(char *out,size_t cap,size_t *used,const char *format,...) {
    if(*used>=cap) {errno=EOVERFLOW;return -1;}
    va_list args;va_start(args,format);int n=vsnprintf(out+*used,cap-*used,format,args);va_end(args);
    if(n<0||(size_t)n>=cap-*used) {errno=EOVERFLOW;return -1;}
    *used+=(size_t)n;return 0;
}
static int owner_json(char *out,size_t cap,size_t *at,const struct ap_grouped_owner *o) {
    return append(out,cap,at,"{\"attempted_sites\":%u,\"event\":\"%s\",\"event_id\":%u,\"group\":\"%s\",\"incarnation\":%" PRIu64 ",\"pending_bytes\":%zu,\"pending_remove\":%u,\"pending_role\":%u,\"phase\":%u,\"verified_sites\":%u,\"write_unknown\":%u}",o->attempted_sites,o->event,o->event_id,o->group,o->incarnation,o->pending_bytes,o->pending_remove,o->pending_role,(unsigned)o->phase,o->verified_sites,o->write_unknown);
}
static int write_json(char *out,size_t cap,size_t *at,const struct ap_grouped_write *w) {
    return append(out,cap,at,"{\"completed\":%d,\"error\":%d,\"raw\":%zd,\"remove\":%u,\"role\":%u,\"started\":%d,\"submitted\":%zu}",w->completed,w->error,w->raw,w->remove,w->role,w->started,w->submitted);
}
int grouped_created_transfer_frame(char *out,size_t cap,struct ap_grouped_creation_pair *pairs,
        struct ap_grouped_owner *complete,char hash[65],uint64_t incarnation,const char *nonce,const char *offer) {
    if(!out||!cap||cap>GA_TRANSFER_BYTES||!pairs||!complete||!hash||!incarnation||!hex32(nonce,1)||!hex32(offer,0)) {errno=EINVAL;return -1;}
    char *transcript=malloc(GA_TRANSFER_BYTES);if(!transcript)return -1;
    struct ap_grouped_owner owner;char proof[AP_GROUPED_SITE_COUNT*AP_GROUPED_LINE_BYTES];size_t at=0,populated=0;
    int result=-1;
    if(ap_grouped_owner_init(&owner,incarnation,nonce,32)||ap_grouped_owner_ack(&owner,incarnation,nonce,32)||append(transcript,GA_TRANSFER_BYTES,&at,"["))goto done;
    for(unsigned i=0;i<AP_GROUPED_SITE_COUNT;i++) {
        struct ap_grouped_creation_pair *p=&pairs[i];memset(p,0,sizeof(*p));
        int n=ap_grouped_create_begin(&owner,proof,populated,i+1,p->line,sizeof(p->line));
        if(n<=0||(size_t)n>sizeof(proof)-populated)goto done;
        p->intent_owner=p->outcome_owner=owner;
        p->intent=(struct ap_grouped_write){.role=i+1,.submitted=(size_t)n};
        p->outcome=p->intent;p->outcome.started=p->outcome.completed=1;p->outcome.raw=n;
        char line[AP_GROUPED_LINE_BYTES*2];static const char digits[]="0123456789abcdef";
        for(int j=0;j<n;j++) {line[j*2]=digits[(unsigned char)p->line[j]>>4];line[j*2+1]=digits[(unsigned char)p->line[j]&15];}line[n*2]=0;
        if(append(transcript,GA_TRANSFER_BYTES,&at,"%s{\"intent\":",i?",":"")||write_json(transcript,GA_TRANSFER_BYTES,&at,&p->intent)||
           append(transcript,GA_TRANSFER_BYTES,&at,",\"intent_owner\":")||owner_json(transcript,GA_TRANSFER_BYTES,&at,&p->intent_owner)||
           append(transcript,GA_TRANSFER_BYTES,&at,",\"line\":\"%s\",\"outcome\":",line)||write_json(transcript,GA_TRANSFER_BYTES,&at,&p->outcome)||
           append(transcript,GA_TRANSFER_BYTES,&at,",\"outcome_owner\":")||owner_json(transcript,GA_TRANSFER_BYTES,&at,&p->outcome_owner)||append(transcript,GA_TRANSFER_BYTES,&at,"}"))goto done;
        memcpy(proof+populated,p->line,(size_t)n);populated+=(size_t)n;
        if(ap_grouped_create_observed(&owner,i+1,n,(size_t)n,proof,populated))goto done;
    }
    if(append(transcript,GA_TRANSFER_BYTES,&at,"]"))goto done;
    unsigned char digest[SHA256_DIGEST_LENGTH];if(!SHA256((const unsigned char *)transcript,at,digest)) {errno=EIO;goto done;}
    for(unsigned i=0;i<sizeof(digest);i++)snprintf(hash+i*2,3,"%02x",digest[i]);
    size_t used=0;
    if(append(out,cap,&used,"{\"incarnation\":%" PRIu64 ",\"nonce\":\"%s\",\"offer\":\"%s\",\"pairs\":%s,\"roles\":[\"CONTROL\",\"PROFILE\",\"EVENTS\"],\"schema\":\"hermit-grouped-created-transfer-v1\",\"transcript_sha256\":\"%s\"}",incarnation,nonce,offer,transcript,hash))goto done;
    *complete=owner;result=(int)used;
done:
    free(transcript);return result;
}
static int validate(struct grouped_adoption_wire *a) {
    if(!a||!a->initialized||a->refused||!a->keeper)return fail(a,a?a->error:EINVAL);
    if(grouped_keeper_wire_validate(a->keeper))return fail(a,errno);
    return 0;
}
int grouped_adoption_wire_init(struct grouped_adoption_wire *a,struct grouped_keeper_wire *keeper) {
    if(!a||a->initialized||!keeper)return fail(a,EINVAL);
    memset(a,0,sizeof(*a));a->keeper=keeper;a->initialized=1;
    for(unsigned i=0;i<3;i++)a->controls[i]=-1;
    a->receive_return=a->send_return=a->ack_return=-1;
    return validate(a);
}
static int wait_packet(struct grouped_adoption_wire *a) {
    for(;;) {
        if(validate(a))return -1;
        struct timespec t;if(clock_gettime(CLOCK_MONOTONIC,&t))return fail(a,errno);
        uint64_t now=(uint64_t)t.tv_sec*1000000000ULL+(uint64_t)t.tv_nsec;
        if(now>=a->keeper->deadline)return fail(a,ETIMEDOUT);
        struct pollfd p={.fd=a->keeper->channel,.events=POLLIN};
        int n=poll(&p,1,(int)((a->keeper->deadline-now+999999ULL)/1000000ULL));
        if(n<0&&errno==EINTR)continue;
        if(n<=0||!(p.revents&POLLIN))return fail(a,n==0?ETIMEDOUT:(n<0?errno:EPIPE));
        return 0;
    }
}
static int receive_packet(struct grouped_adoption_wire *a,char *packet,size_t cap,int controls,int *flags,
        ssize_t *raw,size_t *bytes,int *error,unsigned *called) {
    if(wait_packet(a))return -1;
    union {struct cmsghdr align;unsigned char bytes[CMSG_SPACE(3*sizeof(int))+CMSG_SPACE(sizeof(struct ucred))];} ancillary={0};
    struct iovec iov={packet,cap};struct msghdr m={.msg_iov=&iov,.msg_iovlen=1,.msg_control=ancillary.bytes,.msg_controllen=sizeof(ancillary.bytes)};
    *called=1;errno=0;ssize_t n=recvmsg(a->keeper->channel,&m,MSG_DONTWAIT|MSG_CMSG_CLOEXEC);
    *raw=n;*error=n<0?errno:0;*bytes=n>0?(size_t)n:0;*flags=m.msg_flags;
    if(n<0)return fail(a,errno);
    unsigned rights=0,messages=0,credentials=0;int wrong=m.msg_flags!=MSG_CMSG_CLOEXEC,close_error=0;
    for(struct cmsghdr *c=CMSG_FIRSTHDR(&m);c;c=CMSG_NXTHDR(&m,c)) {
        if(c->cmsg_level==SOL_SOCKET&&c->cmsg_type==SCM_RIGHTS) {
            messages++;size_t bytes=c->cmsg_len>=CMSG_LEN(0)?c->cmsg_len-CMSG_LEN(0):0;wrong|=bytes%sizeof(int)!=0;
            for(size_t i=0;i+sizeof(int)<=bytes;i+=sizeof(int)) {
                int fd;memcpy(&fd,(char *)CMSG_DATA(c)+i,sizeof(fd));
                if(controls&&rights<3) {a->controls[rights]=fd;wrong|=fcntl(fd,F_GETFD)!=FD_CLOEXEC;}
                else if(close(fd)&&!close_error)close_error=errno;
                rights++;
            }
        } else if(c->cmsg_level==SOL_SOCKET&&c->cmsg_type==SCM_CREDENTIALS&&c->cmsg_len==CMSG_LEN(sizeof(struct ucred))) {
            struct ucred peer;memcpy(&peer,CMSG_DATA(c),sizeof(peer));credentials++;
            wrong|=peer.pid!=a->keeper->keeper.pid||peer.uid!=a->keeper->keeper.uid||peer.gid!=a->keeper->keeper.gid;
        } else wrong=1;
    }
    if(wrong||close_error||!n||credentials!=1||rights!=(controls?3U:0U)||messages!=(controls?1U:0U))return fail(a,close_error?close_error:EPROTO);
    if(validate(a))return -1;
    return 0;
}
int grouped_adoption_wire_receive(struct grouped_adoption_wire *a) {
    if(validate(a)||a->receive_attempted)return fail(a,a&&a->error?a->error:EINVAL);
    a->receive_attempted=1;
    if(receive_packet(a,a->packet,sizeof(a->packet),1,&a->receive_flags,
        &a->receive_return,&a->packet_bytes,&a->receive_error,&a->receive_called))return -1;
    char prefix[128];int n=snprintf(prefix,sizeof(prefix),"{\"incarnation\":%" PRIu64 ",\"nonce\":\"%s\",\"offer\":\"",a->keeper->incarnation,a->keeper->nonce);
    if(n<=0||(size_t)n>=sizeof(prefix)||a->packet_bytes<(size_t)n+33||memcmp(a->packet,prefix,(size_t)n))return fail(a,EPROTO);
    memcpy(a->offer,a->packet+n,32);a->offer[32]=0;
    if(!hex32(a->offer,0))return fail(a,EPROTO);
    int count=grouped_created_transfer_frame(a->expected_packet,sizeof(a->expected_packet),a->pairs,&a->expected,a->transcript_sha256,a->keeper->incarnation,a->keeper->nonce,a->offer);
    if(count<0||a->packet_bytes!=(size_t)count||memcmp(a->packet,a->expected_packet,(size_t)count))return fail(a,EPROTO);
    a->received=1;return 0;
}
static int same_owner(const struct ap_grouped_owner *a,const struct ap_grouped_owner *b) {
    return a->incarnation==b->incarnation&&!memcmp(a->group,b->group,sizeof(a->group))&&!memcmp(a->event,b->event,sizeof(a->event))&&a->phase==b->phase&&a->verified_sites==b->verified_sites&&a->attempted_sites==b->attempted_sites&&a->event_id==b->event_id&&a->write_unknown==b->write_unknown&&a->pending_role==b->pending_role&&a->pending_remove==b->pending_remove&&a->pending_bytes==b->pending_bytes;
}
int grouped_adoption_wire_ack(void *context,const struct ap_grouped_owner *owner,const int fd[3],
        const struct ap_grouped_creation_pair *pairs,size_t count) {
    struct grouped_adoption_wire *a=context;
    if(validate(a)||!a->received||a->ack_attempted||!owner||!fd||pairs!=a->pairs||count!=AP_GROUPED_SITE_COUNT||!same_owner(owner,&a->expected))return fail(a,a&&a->error?a->error:EINVAL);
    /* Consume the sole attempt before any uncertain syscall or transmission. */
    a->ack_attempted=1;
    for(unsigned i=0;i<3;i++)if(fd[i]<0||a->controls[i]<0||syscall(SYS_kcmp,getpid(),getpid(),KCMP_FILE,fd[i],a->controls[i])!=0)return fail(a,EPERM);
    int n=snprintf(a->request,sizeof(a->request),"{\"incarnation\":%" PRIu64 ",\"nonce\":\"%s\",\"offer\":\"%s\",\"schema\":\"hermit-grouped-adopt-created-v1\",\"transcript_sha256\":\"%s\"}",a->keeper->incarnation,a->keeper->nonce,a->offer,a->transcript_sha256);
    if(n<=0||(size_t)n>=sizeof(a->request))return fail(a,EOVERFLOW);a->request_bytes=(size_t)n;
    union {struct cmsghdr align;unsigned char bytes[CMSG_SPACE(3*sizeof(int))];} ancillary={0};
    struct iovec iov={a->request,a->request_bytes};struct msghdr m={.msg_iov=&iov,.msg_iovlen=1,.msg_control=ancillary.bytes,.msg_controllen=sizeof(ancillary.bytes)};
    struct cmsghdr *c=CMSG_FIRSTHDR(&m);c->cmsg_level=SOL_SOCKET;c->cmsg_type=SCM_RIGHTS;c->cmsg_len=CMSG_LEN(3*sizeof(int));memcpy(CMSG_DATA(c),fd,3*sizeof(int));
    errno=0;a->send_return=sendmsg(a->keeper->channel,&m,MSG_DONTWAIT|MSG_NOSIGNAL);a->send_error=a->send_return<0?errno:0;
    if(a->send_return!=n)return fail(a,a->send_return<0?errno:EIO);
    if(receive_packet(a,a->reply,sizeof(a->reply),0,&a->ack_flags,
        &a->ack_return,&a->reply_bytes,&a->ack_error,&a->ack_called))return -1;
    char expected[GA_REPLY_BYTES];n=snprintf(expected,sizeof(expected),"{\"incarnation\":%" PRIu64 ",\"nonce\":\"%s\",\"offer\":\"%s\",\"schema\":\"hermit-grouped-adopt-created-ack-v1\",\"transcript_sha256\":\"%s\"}",a->keeper->incarnation,a->keeper->nonce,a->offer,a->transcript_sha256);
    if(n<=0||(size_t)n>=sizeof(expected)||a->reply_bytes!=(size_t)n||memcmp(a->reply,expected,(size_t)n)||validate(a))return fail(a,a->error?a->error:EPROTO);
    a->acknowledged=1;return 0;
}
int grouped_adoption_wire_release(struct grouped_adoption_wire *a) {
    if(!a||!a->initialized) {errno=EINVAL;return -1;}
    int error=0;
    for(unsigned i=0;i<3;i++)if(a->controls[i]>=0) {int fd=a->controls[i];a->controls[i]=-1;if(close(fd)&&!error)error=errno;}
    if(error)return fail(a,error);
    return 0;
}
