/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include <assert.h>
#include <stdarg.h>
#include <stdio.h>
#include "keeper-main.c"
/* Actual keeper loop with every process, socket, clock and session operation
 * substituted. No policy, map, process, socket or descriptor is created. */
static u64 now_ns, deadline;
static int scenario, polls, receives, opens, sends, failures, closes;
static struct ug_frame last_response;
int __wrap_clock_gettime(clockid_t id,struct timespec *out) {
    assert(id==CLOCK_MONOTONIC);
    out->tv_sec=(time_t)(now_ns/1000000000ULL);
    out->tv_nsec=(long)(now_ns%1000000000ULL);return 0;
}
int __wrap_fcntl(int fd,int operation,...) {
    assert(fd==0 && operation==F_DUPFD_CLOEXEC);return 50;
}
int __wrap_close(int fd) {assert(fd==0 || (fd>=70 && fd<=72));closes++;return 0;}
int __wrap_getsockopt(int fd,int level,int option,void *value,socklen_t *length) {
    assert(fd==50 && level==SOL_SOCKET && option==SO_TYPE && *length==sizeof(int));
    *(int *)value=SOCK_SEQPACKET;return 0;
}
long __wrap_syscall(long number,...) {
    if(number==SYS_gettid)return 100;
    assert(number==SYS_pidfd_open);
    va_list args;va_start(args,number);assert(va_arg(args,long)==100);
    assert(va_arg(args,unsigned int)==UG_PIDFD_THREAD && UG_PIDFD_THREAD==0x80);
    va_end(args);return 51;
}
int __wrap_poll(struct pollfd *fds,nfds_t count,int timeout) {
    assert(count==3 && fds[0].fd==50 && timeout>=0 && timeout<=50);
    polls++;assert(polls<=8);
    if(scenario<=2) {
        assert(fds[2].fd==-1);now_ns+=20000000;
        if(scenario==1) {errno=EINTR;return -1;}
        if(scenario==2) {now_ns=deadline;fds[0].revents=POLLIN;return 1;}
        return 0; /* Another actor retains channel: no HUP or readable input. */
    }
    if(polls==1) {fds[0].revents=POLLIN;return 1;}
    /* An alias is still readable, but actual registered parent has died.
     * This happens far beyond the bootstrap deadline after valid INIT. */
    assert(fds[2].fd==63);now_ns+=100000000000ULL;
    fds[0].revents=POLLIN;fds[2].revents=POLLIN;return 2;
}
int ug_channel_receive(int fd,struct ug_packet *request) {
    assert(fd==50);receives++;assert(receives==1);
    memset(request,0,sizeof(*request));request->count=4;
    request->frame=(struct ug_frame){.magic=UG_WIRE_MAGIC,.incarnation=7,
        .sequence=1,.operation=UG_INIT,.rights=4};
    request->frame.values[0]=deadline+(scenario==5?1:0);
    for(u32 i=0;i<4;i++)request->fds[i]=60+(int)i;
    return 0;
}
int ug_channel_send(int fd,const struct ug_packet *reply) {
    assert(fd==50);sends++;last_response=reply->frame;return 0;
}
int ug_session_open(int elf,int bpffs,int recovery,u64 incarnation,struct ug_session **out) {
    assert(elf==60 && bpffs==61 && recovery==62 && incarnation==7);opens++;
    if(scenario==3) {errno=EACCES;return -1;}
    *out=(struct ug_session *)(uintptr_t)123;
    if(scenario==6)now_ns=deadline;
    return 0;
}
int ug_session_readers(struct ug_session *s,int out[3]) {
    assert(s==(void *)123);for(int i=0;i<3;i++)out[i]=70+i;return 0;
}
int ug_session_monitor(struct ug_session *s,int fd,struct ug_monitor_result *out) {
    assert(s==(void *)123 && fd==63);memset(out,0,sizeof(*out));return 0;
}
int ug_session_note_failure(struct ug_session *s,u64 sequence,int error) {
    assert(s==(void *)123 && sequence==1 && error);failures++;return 0;
}
int ug_session_creator_recovery(struct ug_session *s,int *out) {(void)s;(void)out;assert(0);return -1;}
int ug_session_bind_controller(struct ug_session *s,int fd,u64 seq) {(void)s;(void)fd;(void)seq;assert(0);return -1;}
int ug_session_arm_creator(struct ug_session *s,int fd,u64 seq) {(void)s;(void)fd;(void)seq;assert(0);return -1;}
int ug_session_observe_birth(struct ug_session *s,u64 seq,struct ug_birth *out) {(void)s;(void)seq;(void)out;assert(0);return -1;}
int ug_session_register_initial(struct ug_session *s,int fd,u64 seq) {(void)s;(void)fd;(void)seq;assert(0);return -1;}
int ug_session_terminal(struct ug_session *s,u64 seq,struct ug_terminal_receipt *out) {(void)s;(void)seq;(void)out;assert(0);return -1;}
int ug_session_prepare_terminal(struct ug_session *s,u64 seq,struct ug_terminal_receipt *out,int *fd) {(void)s;(void)seq;(void)out;(void)fd;assert(0);return -1;}
int ug_session_close_terminal(struct ug_session *s,u64 seq,u64 proof,u64 ordinal,u64 deadline_ns,struct ug_object_close *out) {(void)s;(void)seq;(void)proof;(void)ordinal;(void)deadline_ns;(void)out;assert(0);return -1;}
static void fresh(int which) {
    scenario=which;now_ns=1000000000ULL;deadline=now_ns+50000000;
    polls=receives=opens=sends=failures=closes=0;memset(&last_response,0,sizeof(last_response));
}
int main(void) {
    unsigned passed=0;
    fresh(0);assert(ug_keeper_main(deadline)==125);assert(polls==3 && !receives && !opens);passed++;
    fresh(1);assert(ug_keeper_main(deadline)==125);assert(polls==3 && !receives && !opens);passed++;
    fresh(2);assert(ug_keeper_main(deadline)==125);assert(polls==1 && !receives && !opens);passed++;
    fresh(3);assert(ug_keeper_main(deadline)==125);assert(opens==1 && sends==1 && last_response.error==EACCES);passed++;
    fresh(4);assert(ug_keeper_main(deadline)==125);assert(opens==1 && sends==1 && !last_response.error && polls==2 && failures==1);passed++;
    fresh(5);assert(ug_keeper_main(deadline)==125);assert(receives==1 && !opens && !sends);passed++;
    fresh(6);assert(ug_keeper_main(deadline)==125);assert(opens==1 && sends==1 && last_response.error==ETIMEDOUT && polls==2);passed++;
    fresh(0);now_ns=deadline;assert(ug_keeper_main(deadline)==125);assert(!closes && !polls);passed++;
    printf("guard_bootstrap_controls=%u passed\n",passed);assert(passed==8);return 0;
}
