/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include "keeper-readback.h"
#include <errno.h>
#include <fcntl.h>
#include <linux/bpf.h>
#include <stdbool.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
static int rb_fail(int error) {errno=error;return -1;}
static int rb_now(uint64_t *out) {
    struct timespec now;if(clock_gettime(CLOCK_MONOTONIC,&now))return -1;
    if(now.tv_sec<0 || now.tv_nsec<0 || now.tv_nsec>=1000000000 ||
       (uint64_t)now.tv_sec>(UINT64_MAX-(uint64_t)now.tv_nsec)/1000000000ULL)return rb_fail(EOVERFLOW);
    *out=(uint64_t)now.tv_sec*1000000000ULL+(uint64_t)now.tv_nsec;return 0;
}
int ug_read_inventory(int fd,struct ug_inventory *out) {
    int seals=fcntl(fd,F_GET_SEALS);
    const int all=F_SEAL_SEAL|F_SEAL_SHRINK|F_SEAL_GROW|F_SEAL_WRITE;
    if(seals<0)return -1;
    if((seals&all)!=all)return rb_fail(EPROTO);
    struct stat st;if(fstat(fd,&st))return -1;
    if(!S_ISREG(st.st_mode) || st.st_size!=(off_t)sizeof(*out))return rb_fail(EPROTO);
    size_t done=0;
    while(done<sizeof(*out)) {
        ssize_t n=pread(fd,(char *)out+done,sizeof(*out)-done,(off_t)done);
        if(n<0 && errno==EINTR)continue;
        if(n<=0)return n<0?-1:rb_fail(EIO);
        done+=(size_t)n;
    }
    if(out->magic!=UG_INVENTORY_MAGIC || !out->incarnation || !out->proof_sequence ||
       !out->record_ordinal || out->count>UG_INVENTORY_MAX)return rb_fail(EPROTO);
    uint32_t counts[3]={0};
    for(uint32_t i=0;i<UG_INVENTORY_MAX;i++) {
        struct ug_plain_id id=out->ids[i];
        if(i>=out->count) {if(id.kind || id.id)return rb_fail(EPROTO);continue;}
        if(id.kind>2 || !id.id)return rb_fail(EPROTO);
        for(uint32_t j=0;j<i;j++)
            if(out->ids[j].kind==id.kind && out->ids[j].id==id.id)return rb_fail(EPROTO);
        counts[id.kind]++;
    }
    if(counts[0]!=out->maps || counts[1]!=out->programs || counts[2]!=out->links ||
       out->maps>10 || out->programs>31 || out->links>31)return rb_fail(EPROTO);
    return 0;
}
int ug_readback_inventory(int fd,const struct ug_object_close *closed,struct ug_readback_receipt *out) {
    if(!closed || !out)return rb_fail(EINVAL);
    memset(out,0,sizeof(*out));
    if(ug_read_inventory(fd,&out->inventory))return -1;
    const struct ug_inventory *ids=&out->inventory;
    if(closed->incarnation!=ids->incarnation || closed->proof_sequence!=ids->proof_sequence ||
       closed->record_ordinal<=ids->record_ordinal || closed->count!=ids->count ||
       !closed->closed_ns || closed->deadline_ns<=closed->closed_ns ||
       closed->deadline_ns-closed->closed_ns>1000000000ULL ||
       closed->first_outcome>2 || closed->guard_faults)return rb_fail(EPROTO);
    out->closed=*closed;
    return ug_query_absence(ids->ids, ids->count, closed->closed_ns,
        closed->deadline_ns, &out->observed_ns, &out->complete_passes);
}
int ug_query_absence(const struct ug_plain_id *ids, uint32_t count,
        uint64_t begin, uint64_t deadline, uint64_t *observed, uint64_t *passes) {
    if (!ids || !observed || !passes || count > UG_INVENTORY_MAX ||
        !begin || deadline <= begin) return rb_fail(EINVAL);
    *observed = *passes = 0;
    for (uint32_t i = 0; i < count; ++i) {
        if (ids[i].kind > 2 || !ids[i].id) return rb_fail(EPROTO);
        for (uint32_t j = 0; j < i; ++j)
            if (ids[i].kind == ids[j].kind && ids[i].id == ids[j].id)
                return rb_fail(EPROTO);
    }
    for(;;) {
        uint64_t now;if(rb_now(&now))return -1;
        if(now<begin || now>=deadline)return rb_fail(ETIMEDOUT);
        bool absent=true;
        for(uint32_t i=0;i<count;i++) {
            if(rb_now(&now))return -1;
            if(now>=deadline)return rb_fail(ETIMEDOUT);
            union bpf_attr a={0};a.map_id=ids[i].id;
            enum bpf_cmd command=ids[i].kind==0?BPF_MAP_GET_FD_BY_ID:
                ids[i].kind==1?BPF_PROG_GET_FD_BY_ID:BPF_LINK_GET_FD_BY_ID;
            int query=(int)syscall(SYS_bpf,command,&a,sizeof(a));
            if(query>=0) {
                /* No query descriptor survives into the next observation. */
                if(close(query))return -1;
                absent=false;
            } else if(errno!=ENOENT)return -1;
        }
        if(rb_now(&now))return -1;
        if(now>=deadline)return rb_fail(ETIMEDOUT);
        *passes=absent?*passes+1:0;
        if(*passes==2) {*observed=now;return 0;}
        struct timespec pause={0,1000000};
        if(nanosleep(&pause,NULL) && errno!=EINTR)return -1;
    }
}
