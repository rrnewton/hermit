/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include <assert.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include "keeper-channel.h"
#include "keeper-readback.h"

/* Compile the actual executable entry. Native transport, parent/helper pidfds,
 * sealed metadata and process lifetime run unchanged. Only privileged BPF ID
 * lookups are substituted; this does not qualify real kernel ID absence. */
#define main readback_executable_main
#include "keeper-readback-main.c"
#undef main

struct query_observations { unsigned calls, bad_command; int error; };
static struct query_observations *observed;
long __real_syscall(long nr,...);
long __wrap_syscall(long nr,...) {
    va_list ap;va_start(ap,nr);long result;
    if(nr==SYS_gettid) result=__real_syscall(nr);
    else if(nr==SYS_pidfd_open) {
        long pid=va_arg(ap,long);unsigned flags=va_arg(ap,unsigned);
        result=__real_syscall(nr,pid,flags);
    } else {
        assert(nr==SYS_bpf);
        int cmd=va_arg(ap,int);union bpf_attr *attr=va_arg(ap,union bpf_attr *);
        size_t bytes=va_arg(ap,size_t);assert(bytes==sizeof(*attr));
        unsigned index=observed->calls++%72;
        int expected=index<10?BPF_MAP_GET_FD_BY_ID:index<41?BPF_PROG_GET_FD_BY_ID:BPF_LINK_GET_FD_BY_ID;
        if(cmd!=expected || attr->map_id!=100+index)observed->bad_command++;
        errno=observed->error?observed->error:ENOENT;result=-1;
    }
    va_end(ap);return result;
}
static uint64_t monotonic_ns(void) {
    struct timespec now;assert(clock_gettime(CLOCK_MONOTONIC,&now)==0);
    return (uint64_t)now.tv_sec*1000000000ULL+(uint64_t)now.tv_nsec;
}
static unsigned census(void) {
    DIR *directory=opendir("/proc/self/fd");assert(directory);
    unsigned count=0;struct dirent *row;
    while((row=readdir(directory)))if(strcmp(row->d_name,".") && strcmp(row->d_name,".."))count++;
    assert(closedir(directory)==0);return count;
}
static bool wait_readable(int fd,uint64_t deadline) {
    for(;;) {
        uint64_t now=monotonic_ns();assert(now<deadline);
        int ms=(int)((deadline-now+999999)/1000000);if(ms>50)ms=50;
        struct pollfd p={fd,POLLIN,0};int n=poll(&p,1,ms);
        if(n<0 && errno==EINTR)continue;
        if(n<0 || (p.revents&(POLLERR|POLLNVAL)))fprintf(stderr,"poll failure fd=%d returned=%d revents=%#x errno=%d\n",fd,n,p.revents,errno);
        assert(n>=0 && !(p.revents&(POLLERR|POLLNVAL)));
        if(n && (p.revents&(POLLIN|POLLHUP)))return (p.revents&POLLIN)!=0;
    }
}
struct run { pid_t pid;int channel,pidfd,helper_pidfd,inventory,parent_pidfd;uint64_t deadline,certificate_deadline; };
static struct ug_inventory full_inventory(void) {
    struct ug_inventory ids={.magic=UG_INVENTORY_MAGIC,.incarnation=7,.proof_sequence=10,
        .record_ordinal=100,.count=72,.maps=10,.programs=31,.links=31};
    for(unsigned i=0;i<72;i++)ids.ids[i]=(struct ug_plain_id){i<10?0:i<41?1:2,100+i};
    return ids;
}
static struct run start_run(uint64_t duration,pid_t parent,bool seal) {
    memset(observed,0,sizeof(*observed));
    struct run r={.channel=-1,.pidfd=-1,.helper_pidfd=-1,.inventory=-1,.parent_pidfd=-1,
        .deadline=monotonic_ns()+duration};
    struct ug_inventory ids=full_inventory();
    r.inventory=memfd_create("guard-original-id-test",MFD_CLOEXEC|MFD_ALLOW_SEALING);assert(r.inventory>=0);
    assert(write(r.inventory,&ids,sizeof(ids))==(ssize_t)sizeof(ids));
    int flags=F_SEAL_SEAL|F_SEAL_SHRINK|F_SEAL_GROW|(seal?F_SEAL_WRITE:0);
    assert(fcntl(r.inventory,F_ADD_SEALS,flags)==0);
    r.parent_pidfd=(int)syscall(SYS_pidfd_open,(long)parent,0U);assert(r.parent_pidfd>=0);
    int channel[2];assert(socketpair(AF_UNIX,SOCK_SEQPACKET|SOCK_CLOEXEC,0,channel)==0);
    r.pid=fork();assert(r.pid>=0);
    if(!r.pid) {
        assert(dup2(channel[1],0)==0);
        for(int fd=3;fd<128;fd++)close(fd);
        char deadline[32];assert(snprintf(deadline,sizeof(deadline),"%llu",(unsigned long long)r.deadline)>0);
        char *argv[]={"readback-test","--readback-before-ns",deadline,NULL};
        _exit(readback_executable_main(3,argv));
    }
    r.channel=channel[0];assert(close(channel[1])==0);
    r.pidfd=(int)syscall(SYS_pidfd_open,(long)r.pid,0U);assert(r.pidfd>=0);
    return r;
}
static struct ug_packet init_packet(struct run *r) {
    struct ug_packet p={.frame={.magic=UG_WIRE_MAGIC,.incarnation=7,.sequence=1,
        .operation=UG_READBACK_INIT,.rights=2,.values={10,r->deadline}},
        .fds={r->inventory,r->parent_pidfd},.count=2};return p;
}
static void send_packet(struct run *r,struct ug_packet p) {assert(ug_channel_send(r->channel,&p)==0);}
static struct ug_packet receive_packet(struct run *r) {
    assert(wait_readable(r->channel,r->deadline));struct ug_packet p;
    assert(ug_channel_receive(r->channel,&p)==0);return p;
}
static void initialized(struct run *r) {
    send_packet(r,init_packet(r));struct ug_packet response=receive_packet(r);
    assert(response.frame.operation==(UG_READBACK_INIT|UG_RESPONSE) && response.frame.sequence==1 &&
        response.frame.incarnation==7 && response.frame.error==0 && response.count==1);
    r->helper_pidfd=response.fds[0];
    char path[64],line[256];assert(snprintf(path,sizeof(path),"/proc/self/fdinfo/%d",r->helper_pidfd)>0);
    FILE *info=fopen(path,"r");assert(info);int found=0,pid;
    while(fgets(line,sizeof(line),info))if(sscanf(line,"Pid:\t%d",&pid)==1) {assert(pid==r->pid);found++;}
    assert(fclose(info)==0 && found==1 && observed->calls==0);
}
static struct ug_packet check_packet(struct run *r) {
    uint64_t now=monotonic_ns(),deadline=now+1000000000ULL;if(deadline>r->deadline)deadline=r->deadline;
    struct ug_packet p={.frame={.magic=UG_WIRE_MAGIC,.incarnation=7,.sequence=2,
        .operation=UG_READBACK_CHECK,.values={7,10,102,now,deadline,72,0,0}}};return p;
}
static void finish_run(struct run *r,int expected) {
    uint64_t end=expected==0?r->certificate_deadline:r->deadline+1000000000ULL;
    assert(end && (expected!=0 || end<=r->deadline));
    assert(wait_readable(r->pidfd,end));
    siginfo_t info={0};assert(waitid(P_PID,r->pid,&info,WEXITED|WNOWAIT)==0);
    assert(info.si_pid==r->pid && info.si_code==CLD_EXITED && info.si_status==expected);
    if(r->helper_pidfd>=0)assert(wait_readable(r->helper_pidfd,end));
    int status;assert(waitpid(r->pid,&status,0)==r->pid && WIFEXITED(status) && WEXITSTATUS(status)==expected);
    int *fds[]={&r->channel,&r->pidfd,&r->helper_pidfd,&r->inventory,&r->parent_pidfd};
    for(unsigned i=0;i<sizeof(fds)/sizeof(fds[0]);i++)if(*fds[i]>=0) {assert(close(*fds[i])==0);*fds[i]=-1;}
    assert(monotonic_ns()<end && observed->bad_command==0);
    printf("owned_helper_reaped pid=%d status=%d queries=%u\n",r->pid,expected,observed->calls);
}
static void checked_success(struct run *r) {
    struct ug_packet request=check_packet(r);send_packet(r,request);struct ug_packet reply=receive_packet(r);
    assert(reply.frame.operation==(UG_READBACK_CHECK|UG_RESPONSE) && reply.frame.sequence==2 &&
        reply.frame.incarnation==7 && !reply.frame.error && !reply.count);
    assert(memcmp(reply.frame.values,request.frame.values,6*sizeof(uint64_t))==0 &&
        reply.frame.values[6]>=request.frame.values[3] && reply.frame.values[6]<request.frame.values[4] &&
        reply.frame.values[7]==2 && observed->calls==144 && monotonic_ns()<request.frame.values[4]);
    r->certificate_deadline=request.frame.values[4];
    finish_run(r,0);
}
/* Recovery runs the same real executable, including its actual actor record.
 * The fixture supplies INVOCATION_ID only in its own forked process; these
 * controls prove channel/lifetime ordering, not systemd authority or BPF IDs. */
static void recovery_capture_control(unsigned mode) {
    memset(observed,0,sizeof(*observed));
    int channel[2],output[2];
    assert(socketpair(AF_UNIX,SOCK_SEQPACKET|SOCK_CLOEXEC,0,channel)==0);
    assert(pipe2(output,O_CLOEXEC|O_NONBLOCK)==0);
    uint64_t deadline=monotonic_ns()+(mode==4?80000000ULL:2000000000ULL);
    int lifetime[2]={-1,-1};pid_t owner=getpid();
    if(mode==6) {
        assert(pipe2(lifetime,O_CLOEXEC)==0);owner=fork();assert(owner>=0);
        if(!owner) {
            close(lifetime[1]);char byte;assert(read(lifetime[0],&byte,1)==0);_exit(0);
        }
        assert(close(lifetime[0])==0);lifetime[0]=-1;
    }
    int owner_pidfd=(int)syscall(SYS_pidfd_open,(long)owner,0U);assert(owner_pidfd>=0);
    pid_t pid=fork();assert(pid>=0);
    if(!pid) {
        assert(dup2(channel[1],0)==0 && dup2(output[1],1)==1);
        for(int fd=3;fd<128;fd++)close(fd);
        assert(setenv("INVOCATION_ID","1234567890abcdef1234567890abcdef",1)==0);
        char end[32],names[72][32];
        assert(snprintf(end,sizeof(end),"%llu",(unsigned long long)deadline)>0);
        char *argv[76]={"recovery-test","--recover-ids-before-ns",end};
        for(unsigned i=0;i<72;i++) {
            assert(snprintf(names[i],sizeof(names[i]),"%u:%u",i<10?0:i<41?1:2,100+i)>0);
            argv[3+i]=names[i];
        }
        _exit(readback_executable_main(75,argv));
    }
    assert(close(channel[1])==0 && close(output[1])==0);
    int pidfd=(int)syscall(SYS_pidfd_open,(long)pid,0U);assert(pidfd>=0);
    unsigned char bootstrap=0;char parent_control[CMSG_SPACE(sizeof(int))]={0};
    struct iovec parent_io={&bootstrap,1};
    struct msghdr parent_message={.msg_iov=&parent_io,.msg_iovlen=1,
        .msg_control=parent_control,.msg_controllen=sizeof(parent_control)};
    struct cmsghdr *parent_right=CMSG_FIRSTHDR(&parent_message);
    parent_right->cmsg_level=SOL_SOCKET;parent_right->cmsg_type=SCM_RIGHTS;
    parent_right->cmsg_len=CMSG_LEN(sizeof(int));memcpy(CMSG_DATA(parent_right),&owner_pidfd,sizeof(owner_pidfd));
    assert(sendmsg(channel[0],&parent_message,MSG_NOSIGNAL)==1);
    char text[4096]={0};size_t used=0;
    while(!memchr(text,'\n',used)) {
        assert(wait_readable(output[0],deadline));
        ssize_t n=read(output[0],text+used,sizeof(text)-1-used);assert(n>0);
        used+=(size_t)n;assert(used<sizeof(text)-1);
    }
    static const char prefix[]="recovery-actor-v1 1234567890abcdef1234567890abcdef ";
    assert(strncmp(text,prefix,sizeof(prefix)-1)==0);
    assert(!strstr(text,"recovery-v1 ") && observed->calls==0);
    struct pollfd alive={pidfd,POLLIN,0};
    assert(poll(&alive,1,20)==0 && observed->calls==0);
    unsigned char bytes[2]={mode==2?2:1,0};
    if(mode==1) {assert(close(channel[0])==0);channel[0]=-1;}
    else if(mode==5) {
        char control[CMSG_SPACE(sizeof(int))]={0};struct iovec io={bytes,1};
        struct msghdr msg={.msg_iov=&io,.msg_iovlen=1,.msg_control=control,.msg_controllen=sizeof(control)};
        struct cmsghdr *c=CMSG_FIRSTHDR(&msg);c->cmsg_level=SOL_SOCKET;c->cmsg_type=SCM_RIGHTS;c->cmsg_len=CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(c),&pidfd,sizeof(pidfd));assert(sendmsg(channel[0],&msg,MSG_NOSIGNAL)==1);
    } else if(mode==6) {assert(close(lifetime[1])==0);lifetime[1]=-1;}
    else if(mode!=4) assert(send(channel[0],bytes,mode==3?2:1,MSG_NOSIGNAL)==(mode==3?2:1));
    assert(wait_readable(pidfd,deadline+1000000000ULL));
    siginfo_t info={0};assert(waitid(P_PID,pid,&info,WEXITED|WNOWAIT)==0);
    int expected=mode==0?0:125;assert(info.si_pid==pid && info.si_code==CLD_EXITED && info.si_status==expected);
    int status;assert(waitpid(pid,&status,0)==pid && WIFEXITED(status) && WEXITSTATUS(status)==expected);
    assert(observed->calls==(mode==0?144U:0U) && observed->bad_command==0);
    assert(close(pidfd)==0 && close(output[0])==0);if(channel[0]>=0)assert(close(channel[0])==0);
    if(mode==6) {
        assert(wait_readable(owner_pidfd,deadline));
        assert(waitpid(owner,&status,0)==owner && WIFEXITED(status) && WEXITSTATUS(status)==0);
    }
    assert(close(owner_pidfd)==0);
    assert(monotonic_ns()<deadline+1000000000ULL);
    printf("recovery_capture_control mode=%u child=%d status=%d queries=%u\n",mode,pid,expected,observed->calls);
}
int main(void) {
    assert(setvbuf(stdout,NULL,_IONBF,0)==0);
    unsigned baseline=census(),passed=0;
    observed=mmap(NULL,sizeof(*observed),PROT_READ|PROT_WRITE,MAP_SHARED|MAP_ANONYMOUS,-1,0);assert(observed!=MAP_FAILED);
    struct run r=start_run(2000000000ULL,getpid(),true);initialized(&r);checked_success(&r);passed++;
    /* The original total deadline can shorten but never extend close+1s. */
    r=start_run(250000000ULL,getpid(),true);initialized(&r);checked_success(&r);passed++;
    r=start_run(2000000000ULL,getpid(),true);initialized(&r);observed->error=EPERM;
    send_packet(&r,check_packet(&r));struct ug_packet response=receive_packet(&r);
    assert(response.frame.error==EPERM && response.count==0 && observed->calls==1);finish_run(&r,125);passed++;
    r=start_run(2000000000ULL,getpid(),false);send_packet(&r,init_packet(&r));finish_run(&r,125);assert(!observed->calls);passed++;
    r=start_run(2000000000ULL,getpid(),true);struct ug_packet p=init_packet(&r);p.frame.incarnation++;
    send_packet(&r,p);finish_run(&r,125);assert(!observed->calls);passed++;
    r=start_run(2000000000ULL,getpid(),true);p=init_packet(&r);p.frame.values[1]++;
    send_packet(&r,p);finish_run(&r,125);assert(!observed->calls);passed++;
    r=start_run(2000000000ULL,getpid(),true);initialized(&r);p=check_packet(&r);p.frame.sequence++;
    send_packet(&r,p);finish_run(&r,125);assert(!observed->calls);passed++;
    r=start_run(2000000000ULL,getpid(),true);initialized(&r);p=check_packet(&r);p.frame.values[2]=100;
    send_packet(&r,p);response=receive_packet(&r);assert(response.frame.error==EPROTO);finish_run(&r,125);assert(!observed->calls);passed++;
    r=start_run(2000000000ULL,getpid(),true);initialized(&r);p=check_packet(&r);p.frame.values[4]++;
    send_packet(&r,p);finish_run(&r,125);assert(!observed->calls);passed++;
    r=start_run(2000000000ULL,getpid(),true);initialized(&r);p=check_packet(&r);
    p.frame.values[3]=monotonic_ns()-1000000001ULL;p.frame.values[4]=p.frame.values[3]+1000000000ULL;
    send_packet(&r,p);response=receive_packet(&r);assert(response.frame.error==ETIMEDOUT);finish_run(&r,125);assert(!observed->calls);passed++;
    /* No INIT: an alias kept open cannot extend the original bootstrap budget. */
    r=start_run(80000000ULL,getpid(),true);finish_run(&r,125);assert(!observed->calls);passed++;
    /* Real held-parent death wins even while this process owns an open channel alias. */
    int lifetime[2];assert(pipe2(lifetime,O_CLOEXEC)==0);pid_t owner=fork();assert(owner>=0);
    if(!owner) {close(lifetime[1]);char byte;assert(read(lifetime[0],&byte,1)==0);_exit(0);}
    assert(close(lifetime[0])==0);
    r=start_run(2000000000ULL,owner,true);initialized(&r);assert(close(lifetime[1])==0);
    assert(wait_readable(r.parent_pidfd,r.deadline));finish_run(&r,125);assert(!observed->calls);
    int status;assert(waitpid(owner,&status,0)==owner && WIFEXITED(status) && WEXITSTATUS(status)==0);passed++;
    /* Controller endpoint loss with parent still live also terminates helper. */
    r=start_run(2000000000ULL,getpid(),true);initialized(&r);assert(close(r.channel)==0);r.channel=-1;
    finish_run(&r,125);assert(!observed->calls);passed++;
    assert(passed==13); /* All original cases remain mandatory and unchanged. */
    unsigned recovery_passed=0;
    for(unsigned mode=0;mode<7;mode++) {recovery_capture_control(mode);recovery_passed++;}
    assert(recovery_passed==7);
    assert(munmap(observed,sizeof(*observed))==0);
    assert(waitpid(-1,NULL,WNOHANG)==-1 && errno==ECHILD);
    unsigned after=census();printf("fd_census baseline=%u after=%u\n",baseline,after);assert(baseline==after);
    printf("readback_main_controls=%u passed; BPF queries substituted; native transport/pidfds/reaping\n",passed);
    assert(passed==13);return 0;
}
