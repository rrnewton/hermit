/* No service launch, global write, BPF open or fresh deadline is hidden here. */
#define _GNU_SOURCE
#include "grouped-guardian-bootstrap.h"
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static uint64_t now_ns(void){struct timespec t;if(clock_gettime(CLOCK_MONOTONIC,&t))return 0;return (uint64_t)t.tv_sec*1000000000ULL+(uint64_t)t.tv_nsec;}
static int fail(struct grouped_guardian_bootstrap *b,int error){
    if(b){b->refused=1;if(!b->error)b->error=error?error:EIO;errno=b->error;}else errno=EINVAL;
    return -1;
}
static int within(struct grouped_guardian_bootstrap *b,uint64_t deadline){
    uint64_t now=now_ns();
    if(!b||b->refused||!now||now>=deadline)return fail(b,ETIMEDOUT);
    if(!b->keeper||deadline>b->keeper->deadline||grouped_keeper_wire_validate(b->keeper))return fail(b,errno?errno:EINVAL);
    return 0;
}
static int receive(struct grouped_guardian_bootstrap *b,struct grouped_keeper_wire *w,
        uint64_t deadline,int *single_right){
    if(single_right)*single_right=-1;
    for(;;){
        if(within(b,deadline)||grouped_keeper_wire_validate(w))return fail(b,errno);
        uint64_t now=now_ns();if(!now||now>=deadline)return fail(b,ETIMEDOUT);
        struct pollfd p={.fd=w->channel,.events=POLLIN};
        int r=poll(&p,1,(int)((deadline-now+999999ULL)/1000000ULL));
        if(r<0&&errno==EINTR)continue;
        if(r<=0||!(p.revents&POLLIN))return fail(b,r==0?ETIMEDOUT:(r<0?errno:EPIPE));
        break;
    }
    union {struct cmsghdr align;unsigned char bytes[CMSG_SPACE(12)+CMSG_SPACE(sizeof(struct ucred))];} ancillary={0};
    struct iovec iov={b->packet,sizeof(b->packet)};
    struct msghdr m={.msg_iov=&iov,.msg_iovlen=1,.msg_control=ancillary.bytes,.msg_controllen=sizeof(ancillary.bytes)};
    b->receive_return=recvmsg(w->channel,&m,MSG_DONTWAIT|MSG_CMSG_CLOEXEC);
    b->receive_flags=m.msg_flags;b->packet_bytes=b->receive_return>0?(size_t)b->receive_return:0;
    if(b->receive_return<0)return fail(b,errno);
    unsigned credentials=0,rights=0,messages=0;int kept=-1,wrong=m.msg_flags!=MSG_CMSG_CLOEXEC,close_error=0;
    for(struct cmsghdr *c=CMSG_FIRSTHDR(&m);c;c=CMSG_NXTHDR(&m,c)){
        if(c->cmsg_level==SOL_SOCKET&&c->cmsg_type==SCM_CREDENTIALS&&c->cmsg_len==CMSG_LEN(sizeof(struct ucred))){
            struct ucred actual;memcpy(&actual,CMSG_DATA(c),sizeof(actual));credentials++;
            wrong|=actual.pid!=w->keeper.pid||actual.uid!=w->keeper.uid||actual.gid!=w->keeper.gid;
        }else if(c->cmsg_level==SOL_SOCKET&&c->cmsg_type==SCM_RIGHTS){
            messages++;size_t bytes=c->cmsg_len>=CMSG_LEN(0)?c->cmsg_len-CMSG_LEN(0):0;
            wrong|=bytes%sizeof(int)!=0;
            for(size_t i=0;i+sizeof(int)<=bytes;i+=sizeof(int)){
                int fd;memcpy(&fd,(char *)CMSG_DATA(c)+i,sizeof(fd));rights++;
                if(single_right&&kept<0)kept=fd;
                else if(close(fd)&&!close_error)close_error=errno;
            }
        }else wrong=1;
    }
    if(credentials!=1||rights!=(single_right?1U:0U)||messages!=(single_right?1U:0U))wrong=1;
    if(wrong||close_error){if(kept>=0){int fd=kept;kept=-1;if(close(fd)&&!close_error)close_error=errno;}return fail(b,close_error?close_error:EPROTO);}
    if(single_right)*single_right=kept;
    return within(b,deadline);
}
static int send_rights(struct grouped_guardian_bootstrap *b,struct grouped_keeper_wire *w,
        const char *packet,size_t bytes,const int *fds,size_t count,uint64_t deadline){
    if(within(b,deadline)||grouped_keeper_wire_validate(w))return fail(b,errno);
    if(!packet||!bytes||bytes>sizeof(b->packet)||!fds||(count!=2&&count!=3))return fail(b,EINVAL);
    union {struct cmsghdr align;unsigned char bytes[CMSG_SPACE(3*sizeof(int))];} ancillary={0};
    struct iovec iov={(void *)packet,bytes};
    struct msghdr m={.msg_iov=&iov,.msg_iovlen=1,.msg_control=ancillary.bytes,.msg_controllen=CMSG_SPACE(count*sizeof(int))};
    struct cmsghdr *c=CMSG_FIRSTHDR(&m);c->cmsg_level=SOL_SOCKET;c->cmsg_type=SCM_RIGHTS;c->cmsg_len=CMSG_LEN(count*sizeof(int));
    memcpy(CMSG_DATA(c),fds,count*sizeof(int));
    b->send_return=sendmsg(w->channel,&m,MSG_DONTWAIT|MSG_NOSIGNAL);
    if(b->send_return!=(ssize_t)bytes)return fail(b,b->send_return<0?errno:EIO);
    return 0;
}
int grouped_guardian_bootstrap_init(struct grouped_guardian_bootstrap *b,
        struct grouped_keeper_wire *keeper,uint64_t cutoff){
    if(!b)return fail(NULL,EINVAL);
    memset(b,0,sizeof(*b));b->keeper=keeper;b->creator_pidfd=b->cgroup_directory=-1;
    b->guardian.channel=b->guardian.keeper_pidfd=-1;b->receive_return=b->send_return=-1;b->creator_cutoff=cutoff;
    uint64_t now=now_ns();
    if(!keeper||!now||cutoff<=now||cutoff-now>1000000000ULL||within(b,cutoff))return fail(b,EINVAL);
    int fd=-1;if(receive(b,keeper,cutoff,&fd)){
        /* receive can discover expiry after accepting the actual endpoint. */
        if(fd>=0)b->guardian.channel=fd;
        return -1;
    }
    b->guardian.channel=fd; /* Retain before any subsequent fallible check. */
    char expected[256];int n=snprintf(expected,sizeof(expected),
        "{\"incarnation\":%" PRIu64 ",\"nonce\":\"%s\",\"schema\":\"hermit-grouped-guardian-channel-v1\",\"stage_deadline\":%" PRIu64 "}",
        keeper->incarnation,keeper->nonce,keeper->deadline);
    if(n<=0||(size_t)n>=sizeof(expected)||b->packet_bytes!=(size_t)n||memcmp(b->packet,expected,(size_t)n)||fcntl(fd,F_GETFD)!=FD_CLOEXEC)return fail(b,EPROTO);
    if(grouped_keeper_wire_init(&b->guardian,fd,keeper->incarnation,keeper->nonce,keeper->deadline))return fail(b,errno);
    if(b->guardian.keeper.pid==keeper->keeper.pid)return fail(b,EPERM);
    b->endpoint_received=1;return 0;
}
static int hex32(const char *p){
    if(!p||strlen(p)!=32)return 0;
    for(unsigned i=0;i<32;i++)if(!((p[i]>='0'&&p[i]<='9')||(p[i]>='a'&&p[i]<='f')))return 0;
    return strspn(p,"0")!=32;
}
int grouped_guardian_bootstrap_creator(struct grouped_guardian_bootstrap *b,const char *unit){
    if(within(b,b?b->creator_cutoff:0))return -1;
    const char *invocation=getenv("INVOCATION_ID");
    if(!b->endpoint_received||b->creator_acknowledged||b->creator_pidfd>=0||!unit||!*unit||strlen(unit)>128||
       strspn(unit,"abcdefghijklmnopqrstuvwxyz0123456789.-")!=strlen(unit)||!hex32(invocation))return fail(b,EINVAL);
    b->creator_pidfd=(int)syscall(SYS_pidfd_open,getpid(),0);if(b->creator_pidfd<0)return fail(b,errno);
    int membership=open("/proc/self/cgroup",O_RDONLY|O_CLOEXEC);if(membership<0)return fail(b,errno);
    char group[4096];ssize_t got=read(membership,group,sizeof(group)-1);int saved=got<0?errno:0;
    if(close(membership)&&!saved)saved=errno;
    if(saved||got<=4||got>=(ssize_t)sizeof(group)-1)return fail(b,saved?saved:EPROTO);
    group[got]=0;if(strncmp(group,"0::/",4)||group[got-1]!='\n'||strchr(group,'\n')!=group+got-1)return fail(b,EPROTO);
    group[got-1]=0;if(!strcmp(group+3,"/")||strstr(group+3,"/../")||strstr(group+3,"/./"))return fail(b,EPROTO);
    char path[4096];int n=snprintf(path,sizeof(path),"/sys/fs/cgroup%s",group+3);
    if(n<=0||(size_t)n>=sizeof(path))return fail(b,EOVERFLOW);
    b->cgroup_directory=open(path,O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC);
    if(b->cgroup_directory<0)return fail(b,errno);
    char packet[512];n=snprintf(packet,sizeof(packet),"UNIT_CREATED unit=%s invocation=%s pid=%ld nonce=%s\n",unit,invocation,(long)getpid(),b->keeper->nonce);
    if(n<=0||(size_t)n>=sizeof(packet))return fail(b,EOVERFLOW);
    int rights[2]={b->creator_pidfd,b->cgroup_directory};
    if(send_rights(b,&b->guardian,packet,(size_t)n,rights,2,b->creator_cutoff)||receive(b,&b->guardian,b->creator_cutoff,NULL))return -1;
    char expected[80];n=snprintf(expected,sizeof(expected),"EXEC %s\n",b->keeper->nonce);
    if(n<=0||(size_t)n>=sizeof(expected)||b->packet_bytes!=(size_t)n||memcmp(b->packet,expected,(size_t)n))return fail(b,EPROTO);
    b->creator_acknowledged=1;return 0;
}
int grouped_guardian_bootstrap_controls(struct grouped_guardian_bootstrap *b,const int fds[3]){
    if(within(b,b?b->creator_cutoff:0))return -1;
    if(!b->creator_acknowledged||b->controls_acknowledged||!fds)return fail(b,EINVAL);
    char packet[1024];int n=snprintf(packet,sizeof(packet),
        "{\"family\":\"controls\",\"incarnation\":%" PRIu64 ",\"nonce\":\"%s\",\"roles\":[{\"name\":\"hermit_kprobe_control\",\"path\":\"/sys/kernel/tracing/kprobe_events\",\"role\":\"CONTROL\"},{\"name\":\"hermit_kprobe_profile\",\"path\":\"/sys/kernel/tracing/kprobe_profile\",\"role\":\"PROFILE\"},{\"name\":\"hermit_trace_events\",\"path\":\"/sys/kernel/tracing/events\",\"role\":\"EVENTS\"}],\"schema\":\"hermit-grouped-roles-v1\"}",b->keeper->incarnation,b->keeper->nonce);
    if(n<=0||(size_t)n>=sizeof(packet))return fail(b,EOVERFLOW);
    for(unsigned i=0;i<3;i++){
        struct stat st;struct statfs fs;int flags=fcntl(fds[i],F_GETFL);
        if(fstat(fds[i],&st)||fstatfs(fds[i],&fs)||st.st_uid||st.st_gid||
           (unsigned long)fs.f_type!=0x74726163UL||fcntl(fds[i],F_GETFD)!=FD_CLOEXEC||flags<0||
           (flags&O_ACCMODE)!=(i?O_RDONLY:O_RDWR)||(i==2?!S_ISDIR(st.st_mode):!S_ISREG(st.st_mode)))return fail(b,EPERM);
    }
    struct grouped_keeper_wire *owners[2]={&b->guardian,b->keeper};
    const char *roles[2]={"guardian","keeper"};
    for(unsigned i=0;i<2;i++){
        if(send_rights(b,owners[i],packet,(size_t)n,fds,3,b->creator_cutoff)||receive(b,owners[i],b->creator_cutoff,NULL))return -1;
        char expected[256];int count=snprintf(expected,sizeof(expected),
            "{\"incarnation\":%" PRIu64 ",\"nonce\":\"%s\",\"role\":\"%s\",\"schema\":\"hermit-grouped-holder-ready-v1\",\"stage_deadline\":%" PRIu64 "}",
            b->keeper->incarnation,b->keeper->nonce,roles[i],b->keeper->deadline);
        if(count<=0||(size_t)count>=sizeof(expected)||b->packet_bytes!=(size_t)count||memcmp(b->packet,expected,(size_t)count))return fail(b,EPROTO);
    }
    b->controls_acknowledged=1;return 0;
}
