/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include <assert.h>
#include <dirent.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include "keeper-main.c"

/* The actual keeper loop and channel implementation run in actual processes.
 * Only ug_session_* policy operations are substituted. These controls prove
 * transport/deadline/held-pidfd lifetime, not BPF load or policy enforcement. */
struct observations { unsigned opens, monitors, failures, recovery; int scenario; };
static volatile struct observations *observed;
struct ug_session { int unused; };
static struct ug_session fake;
int ug_session_open(int elf,int bpffs,int recovery,u64 incarnation,struct ug_session **out) {
    assert(elf>=0 && bpffs>=0 && recovery>=0 && incarnation==7);observed->opens++;
    if(observed->scenario==1) { *out=NULL;errno=EACCES;return -1; }
    *out=&fake;
    if(observed->scenario==2) { errno=EACCES;return -1; }
    return 0;
}
int ug_session_readers(struct ug_session *s,int out[3]) {
    assert(s==&fake);
    for(unsigned i=0;i<3;i++) { out[i]=open("/dev/null",O_RDONLY|O_CLOEXEC);assert(out[i]>=0); }
    if(observed->scenario==3) { errno=EIO;return -1; }
    return 0;
}
int ug_session_monitor(struct ug_session *s,int parent,u64 sequence) {
    assert(s==&fake && parent>=0 && sequence);observed->monitors++;return 0;
}
int ug_session_note_failure(struct ug_session *s,u64 sequence,int error) {
    assert(s==&fake && sequence && error);observed->failures++;return 0;
}
int ug_session_creator_recovery(struct ug_session *s,int *out) {
    assert(s==&fake);observed->recovery++;*out=open("/dev/null",O_RDONLY|O_CLOEXEC);return *out<0?-1:0;
}
int ug_session_bind_controller(struct ug_session *s,int fd,u64 seq) {(void)s;(void)fd;(void)seq;assert(0);return -1;}
int ug_session_arm_creator(struct ug_session *s,int fd,u64 seq) {(void)s;(void)fd;(void)seq;assert(0);return -1;}
int ug_session_observe_birth(struct ug_session *s,u64 seq,struct ug_birth *out) {(void)s;(void)seq;(void)out;assert(0);return -1;}
int ug_session_register_initial(struct ug_session *s,int fd,u64 seq) {(void)s;(void)fd;(void)seq;assert(0);return -1;}
int ug_session_terminal(struct ug_session *s,u64 seq,struct ug_terminal_receipt *out) {(void)s;(void)seq;(void)out;assert(0);return -1;}
int ug_session_prepare_terminal(struct ug_session *s,u64 seq,struct ug_terminal_receipt *out,int *fd) {(void)s;(void)seq;(void)out;(void)fd;assert(0);return -1;}
int ug_session_close_terminal(struct ug_session *s,u64 seq,u64 proof,u64 ordinal,u64 deadline,struct ug_object_close *out) {(void)s;(void)seq;(void)proof;(void)ordinal;(void)deadline;(void)out;assert(0);return -1;}
static u64 now_ns(void) {
    struct timespec now;assert(clock_gettime(CLOCK_MONOTONIC,&now)==0);
    return (u64)now.tv_sec*1000000000ULL+(u64)now.tv_nsec;
}
static unsigned census(void) {
    DIR *d=opendir("/proc/self/fd");assert(d);unsigned n=0;struct dirent *row;
    while((row=readdir(d)))if(strcmp(row->d_name,".") && strcmp(row->d_name,".."))n++;
    assert(closedir(d)==0);return n;
}
static void readable(int fd,u64 deadline) {
    for(;;) {
        u64 now=now_ns();assert(now<deadline);
        int ms=(int)((deadline-now)/1000000ULL);if(ms>50)ms=50;
        struct pollfd p={fd,POLLIN,0};int result=poll(&p,1,ms);
        if(result<0 && errno==EINTR)continue;
        assert(result>=0 && !(p.revents&(POLLERR|POLLNVAL)));
        if(result && (p.revents&POLLIN)) {assert(now_ns()<deadline);return;}
        assert(!(p.revents&POLLHUP));
    }
}
struct actor { pid_t pid;int pipe,pidfd; };
static struct actor start_actor(void) {
    int fds[2];assert(pipe2(fds,O_CLOEXEC)==0);pid_t pid=fork();assert(pid>=0);
    if(!pid) {
        assert(close(fds[1])==0);char byte;assert(read(fds[0],&byte,1)==0);_exit(0);
    }
    assert(close(fds[0])==0);int pidfd=(int)syscall(SYS_pidfd_open,(long)pid,0U);assert(pidfd>=0);
    return (struct actor){pid,fds[1],pidfd};
}
static void finish_actor(struct actor *actor,u64 deadline) {
    readable(actor->pidfd,deadline);siginfo_t info={0};
    assert(waitid(P_PID,actor->pid,&info,WEXITED|WNOWAIT)==0 && info.si_pid==actor->pid && info.si_code==CLD_EXITED && info.si_status==0);
    int status;assert(waitpid(actor->pid,&status,0)==actor->pid && WIFEXITED(status) && !WEXITSTATUS(status));
    assert(close(actor->pidfd)==0 && now_ns()<deadline);
    printf("owned_parent_reaped pid=%d status=0\n",actor->pid);
}
struct run { pid_t pid;int channel,alias,pidfd,self_pidfd,parent_pidfd,input;u64 bootstrap,outer; };
static struct run start_run(pid_t parent,int scenario,u64 bootstrap_duration) {
    *observed=(struct observations){.scenario=scenario};
    u64 start=now_ns();struct run r={.self_pidfd=-1,.bootstrap=start+bootstrap_duration,.outer=start+2000000000ULL};
    r.parent_pidfd=(int)syscall(SYS_pidfd_open,(long)parent,0U);assert(r.parent_pidfd>=0);
    r.input=open("/dev/null",O_RDONLY|O_CLOEXEC);assert(r.input>=0);
    int pair[2];assert(socketpair(AF_UNIX,SOCK_SEQPACKET|SOCK_CLOEXEC,0,pair)==0);
    r.pid=fork();assert(r.pid>=0);
    if(!r.pid) {
        assert(dup2(pair[1],0)==0);for(int fd=3;fd<128;fd++)close(fd);
        _exit(ug_keeper_main(r.bootstrap));
    }
    assert(close(pair[1])==0);r.channel=pair[0];r.alias=fcntl(r.channel,F_DUPFD_CLOEXEC,3);assert(r.alias>=0);
    r.pidfd=(int)syscall(SYS_pidfd_open,(long)r.pid,0U);assert(r.pidfd>=0);return r;
}
static struct ug_packet receive_packet(struct run *r) {
    readable(r->channel,r->outer);struct ug_packet packet;assert(ug_channel_receive(r->channel,&packet)==0);return packet;
}
static void init(struct run *r,int expected) {
    struct ug_packet p={.frame={.magic=UG_WIRE_MAGIC,.incarnation=7,.sequence=1,.operation=UG_INIT,.rights=4},
        .fds={r->input,r->input,r->input,r->parent_pidfd},.count=4};
    p.frame.values[0]=r->bootstrap;assert(ug_channel_send(r->channel,&p)==0);
    p=receive_packet(r);assert(p.frame.operation==(UG_INIT|UG_RESPONSE) && p.frame.incarnation==7 && p.frame.sequence==1 && p.frame.error==expected);
    assert(p.count==(expected?1U:4U));r->self_pidfd=p.fds[0];
    char path[64],line[256];assert(snprintf(path,sizeof(path),"/proc/self/fdinfo/%d",r->self_pidfd)>0);
    FILE *info=fopen(path,"r");assert(info);int found=0,pid;
    while(fgets(line,sizeof(line),info))if(sscanf(line,"Pid:\t%d",&pid)==1) {assert(pid==r->pid);found++;}
    assert(fclose(info)==0 && found==1);
    for(unsigned i=1;i<p.count;i++)assert(close(p.fds[i])==0);
    assert(observed->opens==1);
}
static void finish_run(struct run *r) {
    /* The alias stays open until exact helper terminal observation/reap. */
    readable(r->pidfd,r->outer);siginfo_t info={0};
    assert(waitid(P_PID,r->pid,&info,WEXITED|WNOWAIT)==0 && info.si_pid==r->pid && info.si_code==CLD_EXITED && info.si_status==125);
    if(r->self_pidfd>=0)readable(r->self_pidfd,r->outer);
    int status;assert(waitpid(r->pid,&status,0)==r->pid && WIFEXITED(status) && WEXITSTATUS(status)==125);
    int fds[]={r->channel,r->alias,r->pidfd,r->self_pidfd,r->parent_pidfd,r->input};
    for(unsigned i=0;i<sizeof(fds)/sizeof(fds[0]);i++)if(fds[i]>=0)assert(close(fds[i])==0);
    assert(now_ns()<r->outer);
    printf("owned_keeper_reaped pid=%d status=125 bootstrap_ns=%llu observed_ns=%llu outer_ns=%llu opens=%u failures=%u\n",r->pid,(unsigned long long)r->bootstrap,(unsigned long long)now_ns(),(unsigned long long)r->outer,observed->opens,observed->failures);
}
int main(void) {
    assert(setvbuf(stdout,NULL,_IONBF,0)==0);unsigned baseline=census(),passed=0;
    observed=mmap(NULL,sizeof(*observed),PROT_READ|PROT_WRITE,MAP_SHARED|MAP_ANONYMOUS,-1,0);assert(observed!=MAP_FAILED);
    /* No INIT: another actual endpoint reference cannot renew bootstrap. */
    struct run r=start_run(getpid(),0,80000000ULL);finish_run(&r);
    assert(!observed->opens && now_ns()>=r.bootstrap);passed++;
    /* Actual creator death before INIT cannot leave the aliased helper alive. */
    struct actor actor=start_actor();r=start_run(actor.pid,0,80000000ULL);
    assert(close(actor.pipe)==0);readable(actor.pidfd,r.outer);finish_run(&r);finish_actor(&actor,r.outer);
    assert(!observed->opens && now_ns()>=r.bootstrap);passed++;
    /* Valid INIT changes authority to the held parent, not a guest time cap.
     * A real command after the original bootstrap deadline must still work. */
    actor=start_actor();r=start_run(actor.pid,0,200000000ULL);init(&r,0);
    while(now_ns()<r.bootstrap+20000000ULL) { struct timespec delay={0,1000000};assert(nanosleep(&delay,NULL)==0); }
    struct pollfd live={r.pidfd,POLLIN,0};assert(poll(&live,1,0)==0);
    struct ug_packet p={.frame={.magic=UG_WIRE_MAGIC,.incarnation=7,.sequence=2,.operation=UG_CREATOR_RECOVERY}};
    assert(ug_channel_send(r.channel,&p)==0);p=receive_packet(&r);
    assert(p.frame.operation==(UG_CREATOR_RECOVERY|UG_RESPONSE) && !p.frame.error && p.count==1 && observed->recovery==1);assert(close(p.fds[0])==0);
    assert(close(actor.pipe)==0);readable(actor.pidfd,r.outer);finish_run(&r);finish_actor(&actor,r.outer);
    assert(observed->monitors && observed->failures==1);passed++;
    /* A failure with no session returns its primary error and exits. */
    r=start_run(getpid(),1,200000000ULL);init(&r,EACCES);finish_run(&r);assert(!observed->failures);passed++;
    /* A partial startup owner survives to exact parent death despite aliases. */
    actor=start_actor();r=start_run(actor.pid,2,200000000ULL);init(&r,EACCES);
    assert(close(actor.pipe)==0);readable(actor.pidfd,r.outer);finish_run(&r);finish_actor(&actor,r.outer);
    assert(observed->failures==2);passed++;
    actor=start_actor();r=start_run(actor.pid,3,200000000ULL);init(&r,EIO);
    assert(close(actor.pipe)==0);readable(actor.pidfd,r.outer);finish_run(&r);finish_actor(&actor,r.outer);
    assert(observed->failures==2);passed++;
    assert(munmap((void *)observed,sizeof(*observed))==0);
    assert(waitpid(-1,NULL,WNOHANG)==-1 && errno==ECHILD);
    unsigned after=census();printf("fd_census baseline=%u after=%u\n",baseline,after);assert(baseline==after);
    printf("keeper_lifetime_controls=%u passed; session operations substituted; native main/transport/pidfds/reaping\n",passed);assert(passed==6);return 0;
}
