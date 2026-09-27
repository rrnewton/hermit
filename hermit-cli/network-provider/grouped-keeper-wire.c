/* Existing private socket callback; no tracefs open/write or BPF operation. */
#define _GNU_SOURCE
#include "grouped-keeper-wire.h"
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int bad(void){errno=EINVAL;return -1;}
static uint64_t now_ns(void){struct timespec t;if(clock_gettime(CLOCK_MONOTONIC,&t))return 0;return (uint64_t)t.tv_sec*1000000000ULL+(uint64_t)t.tv_nsec;}
static int nonce_valid(const char *p){
    if(!p||strnlen(p,33)!=32)return 0;
    unsigned nonzero=0;
    for(unsigned i=0;i<32;i++){if(!((p[i]>='0'&&p[i]<='9')||(p[i]>='a'&&p[i]<='f')))return 0;nonzero|=p[i]!='0';}
    return !!nonzero;
}
static int fail(struct grouped_keeper_wire *w,int e){w->refused=1;errno=e?e:EIO;return -1;}
static int live(int fd){struct pollfd p={.fd=fd,.events=POLLIN};int r=poll(&p,1,0);if(r<0)return -1;return r==0;}
static int check(struct grouped_keeper_wire *w){
    uint64_t now=now_ns();
    if(!w||w->refused||w->channel<0||w->keeper_pidfd<0||!now||now>=w->deadline||live(w->keeper_pidfd)!=1)return w?fail(w,ETIMEDOUT):bad();
    struct ucred peer={0};socklen_t n=sizeof(peer);int type=0;socklen_t nt=sizeof(type);
    if(getsockopt(w->channel,SOL_SOCKET,SO_PEERCRED,&peer,&n)||n!=sizeof(peer)||
       peer.pid!=w->keeper.pid||peer.uid!=w->keeper.uid||peer.gid!=w->keeper.gid||
       getsockopt(w->channel,SOL_SOCKET,SO_TYPE,&type,&nt)||nt!=sizeof(type)||type!=SOCK_SEQPACKET)
        return fail(w,EPERM);
    return 0;
}
int grouped_keeper_wire_validate(struct grouped_keeper_wire *w){return check(w);}
int grouped_keeper_wire_init(struct grouped_keeper_wire *w,int channel,uint64_t incarnation,const char *nonce,uint64_t deadline){
    if(!w)return bad();
    memset(w,0,sizeof(*w));w->channel=channel;w->keeper_pidfd=-1;w->send_return=w->receive_return=-1;
    uint64_t now=now_ns();
    if(channel<0||!incarnation||!nonce_valid(nonce)||!now||deadline<=now||deadline-now>20000000000ULL||
       prctl(PR_GET_DUMPABLE,0,0,0,0)!=0||!getuid()||getuid()!=geteuid()||getgid()!=getegid())return fail(w,EINVAL);
    socklen_t n=sizeof(w->keeper);int type=0;socklen_t nt=sizeof(type);
    if(getsockopt(channel,SOL_SOCKET,SO_PEERCRED,&w->keeper,&n)||n!=sizeof(w->keeper)||
       w->keeper.pid<=0||w->keeper.uid!=getuid()||w->keeper.gid!=getgid()||
       getsockopt(channel,SOL_SOCKET,SO_TYPE,&type,&nt)||nt!=sizeof(type)||type!=SOCK_SEQPACKET)
        return fail(w,EPERM);
    w->keeper_pidfd=(int)syscall(SYS_pidfd_open,w->keeper.pid,0);
    if(w->keeper_pidfd<0)return fail(w,errno);
    int enabled=1;
    if(setsockopt(channel,SOL_SOCKET,SO_PASSCRED,&enabled,sizeof(enabled)))return fail(w,errno);
    w->incarnation=incarnation;w->deadline=deadline;w->sequence=1;memcpy(w->nonce,nonce,33);
    return check(w);
}
int grouped_keeper_wire_shorten_deadline(struct grouped_keeper_wire *w,uint64_t deadline){
    if(!w||!deadline||deadline>w->deadline)return w?fail(w,EINVAL):bad();
    /* Latch even an expired original cutoff. No fallback can restart it. */
    w->deadline=deadline;return check(w);
}
int grouped_keeper_journal_frame(char *out,size_t cap,const char *nonce,uint64_t incarnation,uint64_t sequence,
        const struct ap_grouped_owner *o,const struct ap_grouped_write *v,const char *line){
    if(!out||!cap||!nonce_valid(nonce)||!incarnation||!sequence||!o||!v||!line||
       o->incarnation!=incarnation||!v->role||v->role>AP_GROUPED_SITE_COUNT||v->remove>1||
       v->error<0||v->error>4095||v->started<0||v->started>1||v->completed<0||v->completed>1||
       v->started!=v->completed||o->phase!=(v->remove?AP_GROUPED_CLEANING:AP_GROUPED_CREATING)||
       o->pending_role!=v->role||o->pending_remove!=v->remove||o->pending_bytes!=v->submitted)return bad();
    char group[AP_GROUPED_NAME_BYTES],event[AP_GROUPED_NAME_BYTES];
    int ng=snprintf(group,sizeof(group),"hermit_%s",nonce),ne=snprintf(event,sizeof(event),"hermit_classic_%s",nonce);
    if(ng<=0||(size_t)ng>=sizeof(group)||ne<=0||(size_t)ne>=sizeof(event)||
       strnlen(o->group,sizeof(o->group))!=strlen(group)||strcmp(o->group,group)||
       strnlen(o->event,sizeof(o->event))!=strlen(event)||strcmp(o->event,event))return bad();
    size_t len=strnlen(line,AP_GROUPED_LINE_BYTES);
    if(!len||len>=AP_GROUPED_LINE_BYTES||len!=v->submitted)return bad();
    char expected[AP_GROUPED_LINE_BYTES];int count=ap_grouped_command(o,v->role,(int)v->remove,expected,sizeof(expected));
    if(count<=0||(size_t)count!=len||memcmp(expected,line,len))return bad();
    char hex[AP_GROUPED_LINE_BYTES*2];static const char digits[]="0123456789abcdef";
    for(size_t i=0;i<len;i++){hex[i*2]=digits[(unsigned char)line[i]>>4];hex[i*2+1]=digits[(unsigned char)line[i]&15];}hex[len*2]=0;
    int n=snprintf(out,cap,"{\"line\":\"%s\",\"nonce\":\"%s\",\"owner\":{\"attempted_sites\":%u,\"event\":\"%s\",\"event_id\":%u,\"group\":\"%s\",\"incarnation\":%" PRIu64 ",\"pending_bytes\":%zu,\"pending_remove\":%u,\"pending_role\":%u,\"phase\":%u,\"verified_sites\":%u,\"write_unknown\":%u},\"schema\":\"hermit-grouped-journal-v1\",\"sequence\":%" PRIu64 ",\"write\":{\"completed\":%d,\"error\":%d,\"raw\":%zd,\"remove\":%u,\"role\":%u,\"started\":%d,\"submitted\":%zu}}",
       hex,nonce,o->attempted_sites,o->event,o->event_id,o->group,incarnation,o->pending_bytes,o->pending_remove,o->pending_role,(unsigned)o->phase,o->verified_sites,o->write_unknown,sequence,v->completed,v->error,v->raw,v->remove,v->role,v->started,v->submitted);
    if(n<=0||(size_t)n>=cap||(unsigned)n>GK_FRAME_BYTES)return bad();return n;
}
static int receive_ack(struct grouped_keeper_wire *w,const char *expected,size_t wanted){
    for(;;){
        if(check(w))return -1;
        uint64_t now=now_ns();if(!now||now>=w->deadline)return fail(w,ETIMEDOUT);
        uint64_t ms=(w->deadline-now+999999ULL)/1000000ULL;if(ms>INT_MAX)return fail(w,EOVERFLOW);
        struct pollfd p={.fd=w->channel,.events=POLLIN};int r=poll(&p,1,(int)ms);
        if(r<0&&errno==EINTR)continue;
        if(r<=0||!(p.revents&POLLIN))return fail(w,r==0?ETIMEDOUT:(r<0?errno:EPIPE));
        break;
    }
    union{struct cmsghdr align;unsigned char bytes[CMSG_SPACE(sizeof(struct ucred))+CMSG_SPACE(3*sizeof(int))];} ancillary={0};
    struct iovec iov={w->received,sizeof(w->received)};
    struct msghdr msg={.msg_iov=&iov,.msg_iovlen=1,.msg_control=ancillary.bytes,.msg_controllen=sizeof(ancillary.bytes)};
    errno=0;w->receive_return=recvmsg(w->channel,&msg,MSG_CMSG_CLOEXEC|MSG_DONTWAIT);w->receive_error=w->receive_return<0?errno:0;w->receive_flags=msg.msg_flags;
    w->received_bytes=w->receive_return>0?(size_t)w->receive_return:0;
    if(w->receive_return<0)return fail(w,w->receive_error);
    unsigned credentials=0;int wrong=msg.msg_flags!=MSG_CMSG_CLOEXEC,close_error=0;
    for(struct cmsghdr *c=CMSG_FIRSTHDR(&msg);c;c=CMSG_NXTHDR(&msg,c)){
        if(c->cmsg_level==SOL_SOCKET&&c->cmsg_type==SCM_RIGHTS){
            wrong=1;size_t bytes=c->cmsg_len>=CMSG_LEN(0)?c->cmsg_len-CMSG_LEN(0):0;
            for(size_t i=0;i+sizeof(int)<=bytes;i+=sizeof(int)){int fd;memcpy(&fd,(char *)CMSG_DATA(c)+i,sizeof(fd));if(close(fd)&&!close_error)close_error=errno;}
        }else if(c->cmsg_level==SOL_SOCKET&&c->cmsg_type==SCM_CREDENTIALS&&c->cmsg_len==CMSG_LEN(sizeof(struct ucred))){
            struct ucred actual;memcpy(&actual,CMSG_DATA(c),sizeof(actual));credentials++;
            wrong|=actual.pid!=w->keeper.pid||actual.uid!=w->keeper.uid||actual.gid!=w->keeper.gid;
        }else wrong=1;
    }
    if(close_error||wrong||credentials!=1||w->received_bytes!=wanted||memcmp(w->received,expected,wanted))return fail(w,close_error?close_error:EPROTO);
    return check(w);
}
int grouped_keeper_journal(void *context,const struct ap_grouped_owner *owner,const struct ap_grouped_write *write,const char *line){
    struct grouped_keeper_wire *w=context;if(check(w))return -1;
    if(w->sequence==UINT64_MAX)return fail(w,EOVERFLOW);
    int n=grouped_keeper_journal_frame(w->sent,sizeof(w->sent),w->nonce,w->incarnation,w->sequence,owner,write,line);
    if(n<0)return fail(w,errno);w->sent_bytes=(size_t)n;
    w->received_bytes=0;w->receive_return=-1;w->receive_error=w->receive_flags=0;
    errno=0;w->send_return=send(w->channel,w->sent,w->sent_bytes,MSG_DONTWAIT|MSG_NOSIGNAL);w->send_error=w->send_return<0?errno:0;
    if(w->send_return!=n)return fail(w,w->send_error?w->send_error:EIO);
    char expected[GK_ACK_BYTES];int count=snprintf(expected,sizeof(expected),"{\"incarnation\":%" PRIu64 ",\"nonce\":\"%s\",\"schema\":\"hermit-grouped-journal-ack-v1\",\"sequence\":%" PRIu64 "}",w->incarnation,w->nonce,w->sequence);
    if(count<=0||(size_t)count>=sizeof(expected))return fail(w,EOVERFLOW);
    if(receive_ack(w,expected,(size_t)count))return -1;
    w->sequence++;return 0;
}
