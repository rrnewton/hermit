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
static unsigned monitors;
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
    if(scenario==7 && polls==2)return 0;
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
int ug_session_monitor(struct ug_session *s,int fd,u64 sequence) {
    assert(s==(void *)123 && fd==63 && sequence==1);monitors++;
    if(scenario==7 && monitors==1) {errno=EIO;return -1;}
    return 0;
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
static unsigned probe_calls;
static u64 probe_operation;
static int probe_stub(struct ug_session *s,u64 initial,u64 sequence,u64 argument,
                       struct ug_probe_receipt *out,u64 operation) {
    assert(s==(void *)123 && initial==2 && sequence==3);probe_calls++;
    probe_operation=operation;
    *out=(struct ug_probe_receipt){{7,sequence,operation,1,0},initial,argument,UG_PROBE_POLL};
    return 0;
}
int ug_session_probe_arm(struct ug_session *s,u64 initial,u64 seq,u64 kind,struct ug_probe_receipt *out) {
    return probe_stub(s,initial,seq,kind,out,UG_PROBE_ARM);
}
int ug_session_probe_submit(struct ug_session *s,u64 initial,u64 seq,struct ug_probe_receipt *out) {
    return probe_stub(s,initial,seq,0,out,UG_PROBE_SUBMIT);
}
int ug_session_probe_complete(struct ug_session *s,u64 initial,u64 seq,u64 raw,struct ug_probe_receipt *out) {
    return probe_stub(s,initial,seq,raw,out,UG_PROBE_COMPLETE);
}
int ug_session_probe_retire(struct ug_session *s,u64 initial,u64 seq,u64 disposition,struct ug_probe_receipt *out) {
    return probe_stub(s,initial,seq,disposition,out,UG_PROBE_RETIRE);
}
static void probe_dispatch_controls(void) {
    unsigned passed=0;
    for(u32 op=UG_PROBE_ARM;op<=UG_PROBE_RETIRE;op++) {
        struct ug_packet request={0},response={0};
        request.frame.operation=op;request.frame.sequence=3;
        request.frame.values[0]=2;request.frame.values[1]=op==UG_PROBE_ARM?UG_PROBE_POLL:3;
        if(op==UG_PROBE_COMPLETE)request.frame.values[2]=(u64)(int64_t)-516;
        if(op==UG_PROBE_RETIRE)request.frame.values[2]=UG_PROBE_RETIRE_COMPLETED;
        probe_calls=0;assert(probe_request((void *)123,1,&request,&response)==0);
        assert(probe_calls==1 && probe_operation==op && response.frame.values[0]==7 &&
               response.frame.values[1]==3 && response.frame.values[5]==2 &&
               response.frame.values[7]==UG_PROBE_POLL);passed++;
        probe_calls=0;assert(probe_request((void *)123,0,&request,&response)==-1 && errno==EPROTO);
        assert(!probe_calls);passed++;
        request.count=1;assert(probe_request((void *)123,1,&request,&response)==-1 && errno==EPROTO);
        assert(!probe_calls);request.count=0;passed++;
        unsigned used=op==UG_PROBE_ARM || op==UG_PROBE_SUBMIT?2:3;
        for(unsigned i=used;i<8;i++) {
            request.frame.values[i]=1;
            assert(probe_request((void *)123,1,&request,&response)==-1 && errno==EPROTO && !probe_calls);
            request.frame.values[i]=0;passed++;
        }
    }
    struct ug_packet request={0},response={0};probe_calls=0;
    request.frame.operation=UG_INITIAL;
    assert(probe_request((void *)123,1,&request,&response)==-1 && errno==EPROTO && !probe_calls);passed++;
    printf("guard_probe_dispatch_controls=%u passed; session substituted\n",passed);
    assert(passed==35);
}
static void fresh(int which) {
    scenario=which;now_ns=1000000000ULL;deadline=now_ns+50000000;
    polls=receives=opens=sends=failures=closes=0;memset(&last_response,0,sizeof(last_response));
    monitors=0;
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
    printf("guard_bootstrap_controls=%u passed\n",passed);assert(passed==8);
    /* Session substituted here: independently check the maintained main loop
     * keeps calling it after an observed failure instead of disabling it. */
    fresh(7);assert(ug_keeper_main(deadline)==125);
    assert(monitors==2 && polls==3 && opens==1 && sends==1 && failures==1);
    printf("guard_keeper_continued_pump_controls=1 passed; session substituted\n");
    probe_dispatch_controls();return 0;
}
