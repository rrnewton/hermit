/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
/* Actual fork/seqpacket/SCM credentials, live pidfds and same-OFD echo.
 * Ordinary files stand in for controls; no tracefs or keeper-global authority. */
#define _GNU_SOURCE
#include "grouped-adoption-wire.h"
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/kcmp.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
static const char nonce[]="1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a";
static const char offer[]="2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b";
static uint64_t now_ns(void) {struct timespec t;assert(!clock_gettime(CLOCK_MONOTONIC,&t));return (uint64_t)t.tv_sec*1000000000ULL+(uint64_t)t.tv_nsec;}
struct fd_state {int open,flags;dev_t device;ino_t inode;};
static void snapshot(struct fd_state out[128]) {
    for(int fd=0;fd<128;fd++) {
        struct stat s;int flags=fcntl(fd,F_GETFD);out[fd]=(struct fd_state){0};
        if(flags<0) {assert(errno==EBADF);continue;}
        assert(!fstat(fd,&s));out[fd]=(struct fd_state){1,flags,s.st_dev,s.st_ino};
    }
}
static void restored(const struct fd_state before[128]) {
    struct fd_state after[128];snapshot(after);
    for(unsigned i=0;i<128;i++)assert(before[i].open==after[i].open&&before[i].flags==after[i].flags&&before[i].device==after[i].device&&before[i].inode==after[i].inode);
}
static void send_rights(int channel,const void *data,size_t bytes,const int *fds,unsigned count) {
    assert(count<=4);union {struct cmsghdr align;char bytes[CMSG_SPACE(4*sizeof(int))];} control={0};
    struct iovec iov={(void *)data,bytes};struct msghdr m={.msg_iov=&iov,.msg_iovlen=1};
    if(count) {m.msg_control=control.bytes;m.msg_controllen=CMSG_SPACE(count*sizeof(int));struct cmsghdr *c=CMSG_FIRSTHDR(&m);c->cmsg_level=SOL_SOCKET;c->cmsg_type=SCM_RIGHTS;c->cmsg_len=CMSG_LEN(count*sizeof(int));memcpy(CMSG_DATA(c),fds,count*sizeof(int));}
    assert(sendmsg(channel,&m,MSG_DONTWAIT|MSG_NOSIGNAL)==(ssize_t)bytes);
}
struct message {char bytes[GA_REPLY_BYTES];size_t size;int rights[4];unsigned count;};
static struct message receive(int channel,pid_t peer,uint64_t cutoff) {
    struct message result={0};
    for(;;) {
        uint64_t now=now_ns();assert(now<cutoff);struct pollfd p={.fd=channel,.events=POLLIN};
        int n=poll(&p,1,(int)((cutoff-now+999999ULL)/1000000ULL));if(n<0&&errno==EINTR)continue;assert(n==1&&(p.revents&POLLIN));break;
    }
    union {struct cmsghdr align;char bytes[CMSG_SPACE(4*sizeof(int))+CMSG_SPACE(sizeof(struct ucred))];} control={0};
    struct iovec iov={result.bytes,sizeof(result.bytes)};struct msghdr m={.msg_iov=&iov,.msg_iovlen=1,.msg_control=control.bytes,.msg_controllen=sizeof(control.bytes)};
    ssize_t n=recvmsg(channel,&m,MSG_CMSG_CLOEXEC|MSG_DONTWAIT);assert(n>0&&m.msg_flags==MSG_CMSG_CLOEXEC);result.size=(size_t)n;
    unsigned credentials=0,messages=0;
    for(struct cmsghdr *c=CMSG_FIRSTHDR(&m);c;c=CMSG_NXTHDR(&m,c)) {
        if(c->cmsg_level==SOL_SOCKET&&c->cmsg_type==SCM_RIGHTS) {
            messages++;assert(c->cmsg_len>=CMSG_LEN(0));size_t bytes=c->cmsg_len-CMSG_LEN(0);assert(bytes%sizeof(int)==0&&bytes/sizeof(int)<=4);
            result.count=(unsigned)(bytes/sizeof(int));memcpy(result.rights,CMSG_DATA(c),bytes);
            for(unsigned i=0;i<result.count;i++)assert(fcntl(result.rights[i],F_GETFD)==FD_CLOEXEC);
        } else {assert(c->cmsg_level==SOL_SOCKET&&c->cmsg_type==SCM_CREDENTIALS&&c->cmsg_len==CMSG_LEN(sizeof(struct ucred)));struct ucred actual;memcpy(&actual,CMSG_DATA(c),sizeof(actual));assert(actual.pid==peer&&actual.uid==getuid()&&actual.gid==getgid());credentials++;}
    }
    assert(credentials==1&&messages==(result.count?1U:0U));return result;
}
static void done_message(int channel,const char *message) {assert(send(channel,message,strlen(message),MSG_DONTWAIT|MSG_NOSIGNAL)==(ssize_t)strlen(message));}
static int no_write(void *context,const struct ap_grouped_owner *owner,const struct ap_grouped_write *write,const char *line) {(void)context;(void)owner;(void)write;(void)line;assert(0);return -1;}
static void child(int channel,FILE *files[4],unsigned variant,uint64_t cutoff) {
    for(unsigned i=0;i<4;i++)assert(!fclose(files[i]));
    assert(!prctl(PR_SET_DUMPABLE,0,0,0,0));
    struct grouped_keeper_wire keeper;assert(!grouped_keeper_wire_init(&keeper,channel,31,nonce,cutoff));
    struct grouped_adoption_wire *a=calloc(1,sizeof(*a));assert(a);assert(!grouped_adoption_wire_init(a,&keeper));
    int received=grouped_adoption_wire_receive(a),adopted=-1;
    if(!received&&variant==18) {assert(grouped_adoption_wire_receive(a)==-1&&a->refused);received=-1;}
    if(!received) {
        struct ap_grouped_io io={0};for(unsigned i=0;i<AP_GROUP_FDS;i++)io.fd[i]=-1;
        for(unsigned i=0;i<3;i++) {io.fd[i]=fcntl(a->controls[i],F_DUPFD_CLOEXEC,3);assert(io.fd[i]>=0);}
        io.buffer=malloc(AP_GROUPED_CENSUS_BYTES+1);io.proof=malloc(AP_GROUPED_CENSUS_BYTES+1);assert(io.buffer&&io.proof);io.journal=no_write;
        if(variant==17) {
            FILE *copy=tmpfile();assert(copy);
            for(unsigned i=0;i<AP_GROUPED_SITE_COUNT;i++)assert(fwrite(a->pairs[i].line,1,a->pairs[i].intent.submitted,copy)==a->pairs[i].intent.submitted);
            assert(!fflush(copy));assert(!close(io.fd[0]));io.fd[0]=fcntl(fileno(copy),F_DUPFD_CLOEXEC,3);assert(io.fd[0]>=0);assert(!fclose(copy));
        }
        struct ap_grouped_owner owner;assert(!ap_grouped_owner_init(&owner,31,nonce,32));
        adopted=ap_grouped_io_adopt_created(&io,&owner,a->pairs,AP_GROUPED_SITE_COUNT,grouped_adoption_wire_ack,a,cutoff);
        if(variant==0||variant==14) {
            assert(!adopted&&a->acknowledged&&owner.phase==AP_GROUPED_CREATED&&io.writes_count==AP_GROUPED_SITE_COUNT);
            if(variant==14) {ssize_t sent=a->send_return;assert(grouped_adoption_wire_ack(a,&owner,io.fd,a->pairs,AP_GROUPED_SITE_COUNT)==-1&&a->refused&&a->send_return==sent);}
        } else {assert(adopted==-1&&!a->acknowledged&&owner.phase==AP_GROUPED_UNKNOWN&&!owner.attempted_sites&&!io.writes_count);}
        assert(!ap_grouped_io_release(&io));
    } else {
        assert(variant!=0&&variant!=14&&a->refused&&!a->ack_attempted);
        if(variant!=10)assert(a->receive_called&&a->receive_return>0&&!a->receive_error);
        else assert(a->receive_called&&a->receive_return==0&&!a->receive_error);
    }
    if(variant==11||variant==12||variant==19)assert(a->ack_called&&a->ack_return>0&&!a->ack_error);
    if(variant==13)assert(a->error==ETIMEDOUT&&!a->ack_called);
    done_message(channel,adopted==0?"ADOPTED":"REFUSED");
    assert(!grouped_adoption_wire_release(a));assert(!grouped_adoption_wire_release(a));free(a);
    assert(!close(keeper.keeper_pidfd));assert(!close(channel));
    for(int fd=3;fd<128;fd++) {assert(fcntl(fd,F_GETFD)==-1&&errno==EBADF);}
    _exit(0);
}
static void wait_child(pid_t child,uint64_t deadline) {
    int status;
    for(;;) {pid_t got=waitpid(child,&status,WNOHANG);if(got==child)break;assert(got==0&&now_ns()<deadline);struct timespec pause={0,1000000};assert(!nanosleep(&pause,NULL));}
    assert(WIFEXITED(status)&&WEXITSTATUS(status)==0);
}
static void one_case(unsigned variant) {
    struct fd_state before[128];snapshot(before);uint64_t started=now_ns(),cutoff=started+500000000ULL,terminal=started+1000000000ULL;
    FILE *files[4];for(unsigned i=0;i<4;i++) {files[i]=tmpfile();assert(files[i]);}
    struct ap_grouped_owner owner;assert(!ap_grouped_owner_init(&owner,31,nonce,32));
    for(unsigned role=1;role<=AP_GROUPED_SITE_COUNT;role++) {
        char line[AP_GROUPED_LINE_BYTES];int n=ap_grouped_command(&owner,role,0,line,sizeof(line));assert(n>0);assert(fwrite(line,1,(size_t)n,files[0])==(size_t)n);
        assert(fprintf(files[1],"  %-44s %15llu %15llu\n",owner.event,0ULL,0ULL)>0);
    }
    assert(!fflush(files[0])&&!fflush(files[1]));
    char *packet=malloc(GA_TRANSFER_BYTES+1),hash[65];struct ap_grouped_creation_pair *pairs=calloc(AP_GROUPED_SITE_COUNT,sizeof(*pairs));assert(packet&&pairs);
    int length=grouped_created_transfer_frame(packet,GA_TRANSFER_BYTES,pairs,&owner,hash,31,nonce,offer);assert(length>0);
    int channels[2];assert(!socketpair(AF_UNIX,SOCK_SEQPACKET|SOCK_NONBLOCK|SOCK_CLOEXEC,0,channels));int enabled=1;
    for(unsigned i=0;i<2;i++)assert(!setsockopt(channels[i],SOL_SOCKET,SO_PASSCRED,&enabled,sizeof(enabled)));
    pid_t actor=fork();assert(actor>=0);
    if(!actor) {assert(!close(channels[0]));child(channels[1],files,variant,cutoff);}
    assert(!close(channels[1]));int held=(int)syscall(SYS_pidfd_open,actor,0);assert(held>=0);
    int rights[4];for(unsigned i=0;i<4;i++)rights[i]=fileno(files[i]);unsigned count=3;
    switch(variant) {
    case 1:count=2;break;
    case 2:count=4;break;
    case 3:{char *p=strstr(packet,"\"nonce\":\"");assert(p);p[strlen("\"nonce\":\"")]='f';break;}
    case 4:{char *p=strstr(packet,"\"incarnation\":31");assert(p);p[strlen("\"incarnation\":")]='4';break;}
    case 5:{char *p=strstr(packet,"\"pending_role\":1");assert(p);p[strlen("\"pending_role\":")]='2';break;}
    case 6:{char *p=strstr(packet,"\"transcript_sha256\":\"");assert(p);p+=strlen("\"transcript_sha256\":\"");*p=*p=='0'?'1':'0';break;}
    case 7:packet[length++]='x';break;
    case 8:memset(packet,'x',GA_TRANSFER_BYTES+1);length=GA_TRANSFER_BYTES+1;break;
    case 15:rights[0]=fileno(files[1]);rights[1]=fileno(files[0]);break;
    case 16:rights[1]=rights[2]=rights[0];break;
    case 19:{char *p=strstr(packet,"\"offer\":\"");assert(p);p[strlen("\"offer\":\"")]='3';break;}
    default:break;
    }
    if(variant==9) {pid_t wrong=fork();assert(wrong>=0);if(!wrong) {send_rights(channels[0],packet,(size_t)length,rights,count);_exit(0);}wait_child(wrong,terminal);}
    else if(variant==10)assert(!shutdown(channels[0],SHUT_WR));
    else send_rights(channels[0],packet,(size_t)length,rights,count);
    int expects_echo=variant==0||variant==11||variant==12||variant==13||variant==14||variant==19;
    struct message m=receive(channels[0],actor,terminal);
    if(expects_echo) {
        assert(m.count==3);struct pollfd p={.fd=held,.events=POLLIN};assert(poll(&p,1,0)==0);
        for(unsigned i=0;i<3;i++) {assert(syscall(SYS_kcmp,getpid(),getpid(),KCMP_FILE,fileno(files[i]),m.rights[i])==0);assert(!close(m.rights[i]));}
        char expected[GA_REPLY_BYTES];int n=snprintf(expected,sizeof(expected),"{\"incarnation\":31,\"nonce\":\"%s\",\"offer\":\"%s\",\"schema\":\"hermit-grouped-adopt-created-v1\",\"transcript_sha256\":\"%s\"}",nonce,offer,hash);assert(n>0);
        if(variant==19)assert(m.size!=(size_t)n||memcmp(m.bytes,expected,m.size));
        else assert(m.size==(size_t)n&&!memcmp(m.bytes,expected,m.size));
        if(variant!=13) {
            n=snprintf(expected,sizeof(expected),"{\"incarnation\":31,\"nonce\":\"%s\",\"offer\":\"%s\",\"schema\":\"hermit-grouped-adopt-created-ack-v1\",\"transcript_sha256\":\"%s\"}",nonce,offer,hash);assert(n>0);
            if(variant==11)expected[n-3]=expected[n-3]=='0'?'1':'0';
            if(variant==19) {memcpy(expected,"REFUSED",7);n=7;}
            send_rights(channels[0],expected,(size_t)n,rights,variant==12?3:0);
        }
        m=receive(channels[0],actor,terminal);
    }
    const char *final=variant==0||variant==14?"ADOPTED":"REFUSED";
    assert(!m.count&&m.size==strlen(final)&&!memcmp(m.bytes,final,m.size));
    wait_child(actor,terminal);assert(!close(held));assert(!close(channels[0]));
    for(unsigned i=0;i<4;i++)assert(!fclose(files[i]));free(pairs);free(packet);
    errno=0;int status;assert(waitpid(-1,&status,WNOHANG)==-1&&errno==ECHILD);restored(before);assert(now_ns()<terminal);
}
int main(int argc,char **argv) {
    if(argc==2&&!strcmp(argv[1],"--codec")) {
        struct ap_grouped_creation_pair *pairs=calloc(AP_GROUPED_SITE_COUNT,sizeof(*pairs));char *packet=malloc(GA_TRANSFER_BYTES),hash[65];struct ap_grouped_owner owner;assert(pairs&&packet);
        int n=grouped_created_transfer_frame(packet,GA_TRANSFER_BYTES,pairs,&owner,hash,31,nonce,offer);assert(n>0);assert(fwrite(packet,1,(size_t)n,stdout)==(size_t)n);assert(putchar('\n')=='\n');free(packet);free(pairs);return 0;
    }
    assert(argc==1);struct rlimit limit={128,128};assert(!setrlimit(RLIMIT_NOFILE,&limit));
    for(unsigned variant=0;variant<20;variant++)one_case(variant);
    puts("grouped channel:20 actual fork/SCM cases,exact peer credentials/pidfds/OFD echoes,sticky refusals,raw returns and complete child/FD restoration; ordinary files only; no native global authority");return 0;
}
