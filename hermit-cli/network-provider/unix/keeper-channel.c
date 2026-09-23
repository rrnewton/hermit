/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause
 */
#define _GNU_SOURCE
#include "keeper-channel.h"
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdbool.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
static int error(int e) {errno=e;return -1;}
int ug_channel_send(int fd,const struct ug_packet *p) {
    if(!p || p->count>UG_WIRE_RIGHTS || p->frame.rights!=p->count)return error(EINVAL);
    char control[CMSG_SPACE(sizeof(int)*UG_WIRE_RIGHTS)]={0};
    struct iovec io={(void *)&p->frame,sizeof(p->frame)};
    struct msghdr msg={.msg_iov=&io,.msg_iovlen=1};
    if(p->count) {
        msg.msg_control=control;msg.msg_controllen=CMSG_SPACE(sizeof(int)*p->count);
        struct cmsghdr *c=CMSG_FIRSTHDR(&msg);c->cmsg_level=SOL_SOCKET;c->cmsg_type=SCM_RIGHTS;
        c->cmsg_len=CMSG_LEN(sizeof(int)*p->count);memcpy(CMSG_DATA(c),p->fds,sizeof(int)*p->count);
    }
    ssize_t n=sendmsg(fd,&msg,MSG_NOSIGNAL|MSG_DONTWAIT);
    return n==(ssize_t)sizeof(p->frame)?0:(n<0?-1:error(EPROTO));
}
int ug_channel_receive(int fd,struct ug_packet *p) {
    if(!p)return error(EINVAL);
    memset(p,0,sizeof(*p));for(u32 i=0;i<UG_WIRE_RIGHTS;i++)p->fds[i]=-1;
    char control[CMSG_SPACE(sizeof(int)*UG_WIRE_RIGHTS)]={0};
    struct iovec io={&p->frame,sizeof(p->frame)};
    struct msghdr msg={.msg_iov=&io,.msg_iovlen=1,.msg_control=control,.msg_controllen=sizeof(control)};
    ssize_t n=recvmsg(fd,&msg,MSG_CMSG_CLOEXEC|MSG_DONTWAIT);
    if(n<0)return -1;
    bool bad=n!=(ssize_t)sizeof(p->frame) || (msg.msg_flags&(MSG_TRUNC|MSG_CTRUNC));
    for(struct cmsghdr *c=CMSG_FIRSTHDR(&msg);c;c=CMSG_NXTHDR(&msg,c)) {
        if(c->cmsg_level!=SOL_SOCKET || c->cmsg_type!=SCM_RIGHTS || c->cmsg_len<CMSG_LEN(0)) {bad=true;continue;}
        size_t bytes=c->cmsg_len-CMSG_LEN(0);
        if(bytes%sizeof(int))bad=true;
        size_t count=bytes/sizeof(int);
        if(count>UG_WIRE_RIGHTS-p->count) {bad=true;count=UG_WIRE_RIGHTS-p->count;}
        memcpy(p->fds+p->count,CMSG_DATA(c),count*sizeof(int));p->count+=(u32)count;
    }
    if(bad || p->frame.magic!=UG_WIRE_MAGIC || !p->frame.incarnation ||
       p->frame.rights!=p->count || p->frame.reserved)return error(EPROTO);
    return 0;
}
static int wait_until(int fd,short events,u64 deadline) {
    for(;;) {
        struct timespec now;if(clock_gettime(CLOCK_MONOTONIC,&now))return -1;
        u64 nanos=(u64)now.tv_sec*1000000000ULL+(u64)now.tv_nsec;
        if(nanos>=deadline)return error(ETIMEDOUT);
        u64 remaining=deadline-nanos;
        int ms=remaining>=50000000ULL?50:(int)((remaining+999999ULL)/1000000ULL);
        struct pollfd p={fd,events,0};int n=poll(&p,1,ms);
        if(n<0 && errno==EINTR)continue;
        if(n<0)return -1;
        if(p.revents&(POLLNVAL|POLLERR))return error(EIO);
        if(p.revents&events)return 0;
        if(p.revents&POLLHUP)return error(EPIPE);
    }
}
int ug_channel_request_observed(int fd,const struct ug_packet *request,struct ug_packet *response,
                                u64 deadline,int *verified_response) {
    if(verified_response)*verified_response=0;
    if(!request || !response)return error(EINVAL);
    memset(response,0,sizeof(*response));for(u32 i=0;i<UG_WIRE_RIGHTS;i++)response->fds[i]=-1;
    if(wait_until(fd,POLLOUT,deadline) || ug_channel_send(fd,request))return -1;
    /* Send happens once. A lost response leaves the operation submitted. */
    if(wait_until(fd,POLLIN,deadline) || ug_channel_receive(fd,response))return -1;
    if(response->frame.incarnation!=request->frame.incarnation ||
       response->frame.sequence!=request->frame.sequence ||
       response->frame.operation!=(request->frame.operation|UG_RESPONSE))return error(EPROTO);
    if(verified_response)*verified_response=1;
    return response->frame.error?error(response->frame.error):0;
}
int ug_channel_request(int fd,const struct ug_packet *request,struct ug_packet *response,u64 deadline) {
    return ug_channel_request_observed(fd,request,response,deadline,NULL);
}

/* Never called in the child. These are delegated map-FD operations, not a new
 * BPF load/attach or a numeric-PID authority. The exact alive creator is the
 * only possible kernel user of this task-storage entry during this call. */
int ug_creator_terminalize(int map,int creator,int helper,u64 incarnation,
                           u64 sequence,int acknowledged,struct ug_birth *out) {
    if(map<0 || creator<0 || helper<0 || !incarnation || !sequence ||
       (acknowledged!=0 && acknowledged!=1) || !out)return error(EINVAL);
    memset(out,0,sizeof(*out));
    if(!acknowledged) {
        struct pollfd p={helper,POLLIN,0};int n=poll(&p,1,0);
        if(n<0)return -1;
        if(p.revents&(POLLERR|POLLNVAL))return error(EIO);
        if(n!=1 || !(p.revents&POLLIN))return error(EAGAIN);
    }
    union bpf_attr a={0};a.map_fd=map;a.key=(u64)(uintptr_t)&creator;
    a.value=(u64)(uintptr_t)out;
    if(syscall(SYS_bpf,BPF_MAP_LOOKUP_ELEM,&a,sizeof(a))) {
        /* A lost reply + dead sole writer permits the no-insertion outcome.
         * ACK followed by absence is an inconsistent lifecycle, not success. */
        if(errno==ENOENT && !acknowledged)return UG_CREATOR_ABSENT;
        return -1;
    }
    if(out->incarnation!=incarnation || out->sequence!=sequence || out->in_copy)
        return error(EPROTO);
    if(out->phase==UG_BIRTH_COMMITTED || out->phase==UG_BIRTH_FAILED) {
        if(out->phase==UG_BIRTH_COMMITTED && (!out->object || !out->generation || !out->cookie))
            return error(EPROTO);
        return UG_CREATOR_CONSUMED; /* inert at the exact copy_net_ns hook */
    }
    if(out->phase!=UG_BIRTH_ARMED || out->object || out->generation || out->cookie)
        return error(EPROTO);
    a.value=0;
    if(syscall(SYS_bpf,BPF_MAP_DELETE_ELEM,&a,sizeof(a)))return -1;
    /* Deletion is not the receipt: read the same exact task-storage key back. */
    struct ug_birth remaining={0};a.value=(u64)(uintptr_t)&remaining;
    if(!syscall(SYS_bpf,BPF_MAP_LOOKUP_ELEM,&a,sizeof(a)))return error(EPROTO);
    return errno==ENOENT?UG_CREATOR_REMOVED:-1;
}

int ug_creator_mask_block(struct ug_creator_mask *mask) {
#if !defined(__linux__) || !defined(__x86_64__)
    (void)mask;return error(ENOTSUP);
#else
    if(!mask || mask->active)return error(EINVAL);
    long tid=syscall(SYS_gettid);if(tid<=0 || tid>INT32_MAX)return error(EIO);
    u64 all=~(u64)0,old=0;
    if(syscall(SYS_rt_sigprocmask,SIG_SETMASK,&all,&old,sizeof(all)))return -1;
    *mask=(struct ug_creator_mask){.original=old,.tid=(s32)tid,.active=1};return 0;
#endif
}
int ug_creator_mask_restore(struct ug_creator_mask *mask,int child_branch) {
#if !defined(__linux__) || !defined(__x86_64__)
    (void)mask;(void)child_branch;return error(ENOTSUP);
#else
    if(!mask || mask->active!=1 || (child_branch!=0 && child_branch!=1))return error(EINVAL);
    /* Only the actual Container child callback may select child_branch. Its
     * namespace-visible TID is not comparable with the outside creator's TID. */
    if(!child_branch && syscall(SYS_gettid)!=mask->tid)return error(EPROTO);
    if(syscall(SYS_rt_sigprocmask,SIG_SETMASK,&mask->original,NULL,sizeof(mask->original)))return -1;
    mask->active=0;return 0;
#endif
}
