/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include <assert.h>
#include <stdarg.h>
#include <stdio.h>
#include "keeper-readback.c"
/* Actual query implementation; all syscalls/FDs/time are substituted. */
static struct ug_inventory inventory;
static uint64_t now_ns,step;
static int calls,closes,live,error_query,error_close,seals;
int __wrap_fcntl(int fd,int cmd,...) {assert(fd==50 && cmd==F_GET_SEALS);return seals;}
int __wrap_fstat(int fd,struct stat *out) {assert(fd==50);memset(out,0,sizeof(*out));out->st_mode=S_IFREG;out->st_size=sizeof(inventory);return 0;}
ssize_t __wrap_pread(int fd,void *out,size_t size,off_t offset) {assert(fd==50 && !offset && size==sizeof(inventory));memcpy(out,&inventory,size);return (ssize_t)size;}
int __wrap_clock_gettime(clockid_t id,struct timespec *out) {assert(id==CLOCK_MONOTONIC);now_ns+=step;out->tv_sec=now_ns/1000000000;out->tv_nsec=now_ns%1000000000;return 0;}
int __wrap_nanosleep(const struct timespec *requested,struct timespec *remaining) {(void)remaining;assert(!requested->tv_sec && requested->tv_nsec==1000000);now_ns+=1000000;return 0;}
long __wrap_syscall(long nr,...) {
    assert(nr==SYS_bpf);va_list ap;va_start(ap,nr);int cmd=va_arg(ap,int);union bpf_attr *a=va_arg(ap,union bpf_attr *);assert(va_arg(ap,size_t)==sizeof(*a));va_end(ap);
    int index=calls++%3;assert(a->map_id==(uint32_t)(100+index));
    assert(cmd==(index==0?BPF_MAP_GET_FD_BY_ID:index==1?BPF_PROG_GET_FD_BY_ID:BPF_LINK_GET_FD_BY_ID));
    if(error_query) {errno=error_query;return -1;}
    if(live && calls==1)return 80;
    errno=ENOENT;return -1;
}
int __wrap_close(int fd) {assert(fd==80);closes++;if(error_close) {errno=EIO;return -1;}return 0;}
static struct ug_object_close fresh(void) {
    memset(&inventory,0,sizeof(inventory));inventory=(struct ug_inventory){.magic=UG_INVENTORY_MAGIC,
        .incarnation=7,.proof_sequence=10,.record_ordinal=100,.count=3,.maps=1,.programs=1,.links=1};
    for(uint32_t i=0;i<3;i++)inventory.ids[i]=(struct ug_plain_id){i,100+i};
    now_ns=1000000000;step=0;calls=closes=live=error_query=error_close=0;
    seals=F_SEAL_SEAL|F_SEAL_SHRINK|F_SEAL_GROW|F_SEAL_WRITE;
    return (struct ug_object_close){7,10,102,1000000000,2000000000,3,0,0};
}
int main(void) {
    unsigned passed=0;struct ug_readback_receipt receipt;struct ug_object_close closed=fresh();
    assert(ug_readback_inventory(50,&closed,&receipt)==0 && calls==6 && !closes && receipt.complete_passes==2);passed++;
    closed=fresh();live=1;assert(ug_readback_inventory(50,&closed,&receipt)==0 && calls==9 && closes==1);passed++;
    closed=fresh();error_query=EPERM;assert(ug_readback_inventory(50,&closed,&receipt)==-1 && errno==EPERM && calls==1);passed++;
    closed=fresh();seals&=~F_SEAL_WRITE;assert(ug_readback_inventory(50,&closed,&receipt)==-1 && errno==EPROTO && !calls);passed++;
    closed=fresh();inventory.ids[1]=inventory.ids[0];assert(ug_readback_inventory(50,&closed,&receipt)==-1 && errno==EPROTO && !calls);passed++;
    closed=fresh();closed.incarnation++;assert(ug_readback_inventory(50,&closed,&receipt)==-1 && errno==EPROTO && !calls);passed++;
    closed=fresh();now_ns=closed.deadline_ns;assert(ug_readback_inventory(50,&closed,&receipt)==-1 && errno==ETIMEDOUT && !calls);passed++;
    closed=fresh();closed.deadline_ns=1000000010;step=2;assert(ug_readback_inventory(50,&closed,&receipt)==-1 && errno==ETIMEDOUT && calls==3 && !receipt.complete_passes);passed++;
    closed=fresh();live=error_close=1;assert(ug_readback_inventory(50,&closed,&receipt)==-1 && errno==EIO && closes==1);passed++;
    closed=fresh();closed.deadline_ns++;assert(ug_readback_inventory(50,&closed,&receipt)==-1 && errno==EPROTO && !calls);passed++;
    printf("guard_readback_controls=%u passed\n",passed);assert(passed==10);return 0;
}
