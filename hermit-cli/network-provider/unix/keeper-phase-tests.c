/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define main retained_terminal_controls
#define __wrap_syscall retained_syscall
#define __wrap_write retained_write
#include "keeper-terminal-tests.c"
#undef main
#undef __wrap_syscall
#undef __wrap_write
static u64 clock_ns;
static int queries,clock_failure,clock_calls,exported;
static struct ug_inventory exported_inventory;
struct bpf_map *bpf_object__next_map(const struct bpf_object *object,const struct bpf_map *map) {
    assert(object==(void *)123);uintptr_t n=(uintptr_t)map;
    return n==UG_MAPS?NULL:(void *)(n+1);
}
int bpf_map__fd(const struct bpf_map *map) {return 199+(int)(uintptr_t)map;}
struct bpf_program *bpf_object__next_program(const struct bpf_object *object,struct bpf_program *program) {
    assert(object==(void *)123);uintptr_t n=(uintptr_t)program;
    return n==UG_LINKS?NULL:(void *)(n+1);
}
int bpf_program__fd(const struct bpf_program *program) {return 499+(int)(uintptr_t)program;}
int bpf_link__fd(const struct bpf_link *link) {return 199+UG_MAPS+(int)(uintptr_t)link;}
long __wrap_syscall(long nr,...) {
    va_list ap;va_start(ap,nr);
    if(nr==SYS_memfd_create) {va_end(ap);exported++;return 800;}
    assert(nr==SYS_bpf);int cmd=va_arg(ap,int);union bpf_attr *a=va_arg(ap,union bpf_attr *);
    size_t size=va_arg(ap,size_t);va_end(ap);
    if(cmd==BPF_LINK_GET_FD_BY_ID || cmd==BPF_MAP_GET_FD_BY_ID || cmd==BPF_PROG_GET_FD_BY_ID)queries++;
    if(cmd==BPF_OBJ_GET_INFO_BY_FD && a->info.bpf_fd>=500 && a->info.bpf_fd<531) {
        ((struct bpf_prog_info *)(uintptr_t)a->info.info)->id=2000+a->info.bpf_fd-500;return 0;
    }
    return retained_syscall(nr,cmd,a,size);
}
ssize_t __wrap_write(int fd,const void *bytes,size_t size) {
    if(fd==800) {assert(size==sizeof(exported_inventory));memcpy(&exported_inventory,bytes,size);return (ssize_t)size;}
    return retained_write(fd,bytes,size);
}
int __wrap_fcntl(int fd,int cmd,...) {assert(fd==800 && cmd==F_ADD_SEALS);return 0;}
int __wrap_clock_gettime(clockid_t id,struct timespec *out) {
    assert(id==CLOCK_MONOTONIC);clock_calls++;
    if(clock_failure && clock_calls==clock_failure) {errno=EIO;return -1;}
    out->tv_sec=(time_t)(clock_ns/1000000000);out->tv_nsec=(long)(clock_ns%1000000000);return 0;
}
static struct ug_terminal_receipt prepare(void) {
    fresh();queries=clock_failure=clock_calls=exported=0;clock_ns=1000000000;
    struct ug_terminal_receipt proof;int inventory=-1;
    assert(ug_session_prepare_terminal(&subject,10,&proof,&inventory)==0);
    assert(inventory==800 && exported==1 && queries==0 && !object_closed);
    assert(subject.close_prepared && !subject.released && destroys==31 && unlinks==41);
    assert(exported_inventory.count==72 && exported_inventory.maps==10 &&
           exported_inventory.programs==31 && exported_inventory.links==31);
    assert(exported_inventory.proof_sequence==10 && exported_inventory.record_ordinal==proof.record_ordinal);
    return proof;
}
int main(void) {
    assert(retained_terminal_controls()==0);unsigned passed=0;
    struct ug_terminal_receipt proof=prepare();struct ug_object_close closed;
    assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal,4000000000,&closed)==0);
    assert(object_closed && !dir_present && queries==0 && closed.count==72);
    assert(closed.closed_ns==1000000000 && closed.deadline_ns==2000000000);
    assert(closed.record_ordinal>proof.record_ordinal);passed++;
    proof=prepare();assert(ug_session_close_terminal(&subject,11,9,proof.record_ordinal,4000000000,&closed)==-1 && errno==EINVAL);
    assert(!object_closed && !subject.close_submitted);passed++;
    proof=prepare();assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal+1,4000000000,&closed)==-1 && errno==EINVAL);
    assert(!object_closed);passed++;
    proof=prepare();assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal,clock_ns,&closed)==-1 && errno==ETIMEDOUT);
    assert(!object_closed);passed++;
    proof=prepare();assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal,1500000000,&closed)==0);
    assert(closed.deadline_ns==1500000000);passed++;
    proof=prepare();clock_failure=2;
    assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal,4000000000,&closed)==-1 && errno==EIO);
    assert(object_closed && subject.close_submitted);clock_failure=0;
    assert(ug_session_close_terminal(&subject,12,10,proof.record_ordinal,9000000000,&closed)==-1 && errno==EIO);passed++;
    proof=prepare();subject.journal_error=EIO;
    assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal,4000000000,&closed)==-1 && errno==EIO);
    assert(!object_closed);passed++;
    proof=prepare();assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal,4000000000,&closed)==0);
    assert(ug_session_close_terminal(&subject,12,10,proof.record_ordinal,9000000000,&closed)==-1 && errno==EBUSY);passed++;
    printf("guard_two_phase_controls=%u passed\n",passed);assert(passed==8);return 0;
}
