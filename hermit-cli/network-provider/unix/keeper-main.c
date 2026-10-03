/* SPDX-License-Identifier: GPL-2.0 */
#define _GNU_SOURCE
#include "keeper-channel.h"
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdbool.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <time.h>
/* Exec-created outside helper. Parent and controller have separate actual
 * endpoints and sequence spaces. Only the parent lane can close admissions or
 * release policy after the exact aggregate proof. EOF never supplies proof. */
/* The deadline is supplied before the launcher is forked. It cannot be renewed
 * by poll wakeups, channel aliases, EINTR, malformed commands or slow exec. */
static int bootstrap_remaining(u64 deadline) {
    struct timespec now;
    if(clock_gettime(CLOCK_MONOTONIC,&now))return -1;
    if(now.tv_sec<0 || now.tv_nsec<0 || now.tv_nsec>=1000000000 ||
       (u64)now.tv_sec>(UINT64_MAX-(u64)now.tv_nsec)/1000000000ULL) {errno=EOVERFLOW;return -1;}
    u64 current=(u64)now.tv_sec*1000000000ULL+(u64)now.tv_nsec;
    if(!deadline || current>=deadline) {errno=ETIMEDOUT;return -1;}
    u64 remaining=deadline-current;
    /* Round DOWN: the next loop observes the original deadline, never a new
     * 50ms budget. A sub-millisecond tail uses a nonblocking poll. */
    return remaining>=50000000ULL?50:(int)(remaining/1000000ULL);
}
/* Commands carry no new rights. Only the already authenticated controller
 * lane can act on its retained INITIAL owner; unused wire words are zero. */
static int probe_request(struct ug_session *session,int lane,
                         const struct ug_packet *request,struct ug_packet *response) {
    if(!lane || request->count)return (errno=EPROTO),-1;
    u32 op=request->frame.operation;
    u32 used=(op==UG_PROBE_ARM || op==UG_PROBE_SUBMIT)?2:3;
    for(u32 i=used;i<8;i++)if(request->frame.values[i])return (errno=EPROTO),-1;
    const u64 *v=request->frame.values;struct ug_probe_receipt receipt;int result;
    switch(op) {
    case UG_PROBE_ARM:
        result=ug_session_probe_arm(session,v[0],request->frame.sequence,v[1],&receipt);break;
    case UG_PROBE_SUBMIT:
        result=ug_session_probe_submit(session,v[0],v[1],&receipt);break;
    case UG_PROBE_COMPLETE:
        result=ug_session_probe_complete(session,v[0],v[1],v[2],&receipt);break;
    case UG_PROBE_RETIRE:
        result=ug_session_probe_retire(session,v[0],v[1],v[2],&receipt);break;
    default:return (errno=EPROTO),-1;
    }
    _Static_assert(sizeof(receipt)==sizeof(response->frame.values),"probe wire receipt");
    if(!result)memcpy(response->frame.values,&receipt,sizeof(receipt));
    return result;
}
int ug_keeper_main(u64 bootstrap_deadline_ns) {
    if(bootstrap_remaining(bootstrap_deadline_ns)<0)return 125;
    int parent=fcntl(STDIN_FILENO,F_DUPFD_CLOEXEC,3);if(parent<0)return 125;
    if(close(STDIN_FILENO))return 125;
    int type=0;socklen_t size=sizeof(type);
    if(getsockopt(parent,SOL_SOCKET,SO_TYPE,&type,&size) || type!=SOCK_SEQPACKET)return 125;
    int self_pidfd=(int)syscall(SYS_pidfd_open,syscall(SYS_gettid),UG_PIDFD_THREAD);
    if(self_pidfd<0)return 125;
    struct ug_session *session=NULL;u64 incarnation=0,last_sequence[2]={0};
    int parent_pidfd=-1,controller=-1;bool controller_registered=false;
    bool controller_closed=false;
    u64 terminal_deadline=0;struct ug_terminal_receipt terminal_prepared={0};
    int retained[4*(UG_MAX_INITIAL_TASKS+8)];u32 retained_count=0;
    for(;;) {
        if(session && parent_pidfd>=0) {
            /* Session bookkeeping preserves keeper-local origin and secondary
             * failure after policy. Keep the parent recovery lane alive. */
            ug_session_monitor(session,parent_pidfd,last_sequence[0]);
        }
        int timeout=50;
        if(parent_pidfd<0) {
            timeout=bootstrap_remaining(bootstrap_deadline_ns);
            if(timeout<0)return 125;
        }
        struct pollfd ready[3]={{parent,POLLIN,0},{controller,POLLIN,0},
                               {parent_pidfd,POLLIN,0}};
        int n=poll(ready,3,timeout);
        if(n<0 && errno==EINTR)continue;
        /* A ready packet observed after the deadline cannot authorize INIT. */
        if(parent_pidfd<0 && bootstrap_remaining(bootstrap_deadline_ns)<0)return 125;
        if(n<0 || ready[2].revents&(POLLIN|POLLERR|POLLNVAL|POLLHUP)) {
            if(session)ug_session_note_failure(session,last_sequence[0],n<0?errno:EPIPE);
            return 125; /* Parent lost: pins and durable record remain. */
        }
        if(ready[0].revents&(POLLERR|POLLNVAL|POLLHUP)) {
            if(session)ug_session_note_failure(session,last_sequence[0],EPIPE);
            return 125;
        }
        if(controller>=0 && ready[1].revents&(POLLERR|POLLNVAL|POLLHUP)) {
            /* File-table teardown can precede actual task terminal. Keep the
             * parent lane; ug_session_terminal independently checks its pidfd. */
            controller_closed=true;controller=-1;
        }
        int lane=ready[0].revents&POLLIN?0:
                 (!controller_closed && ready[1].revents&POLLIN?1:-1);
        if(lane<0)continue;
        int channel=lane?controller:parent;
        struct ug_packet request,response;
        int received=ug_channel_receive(channel,&request);
        for(u32 i=0;i<request.count;i++) {
            if(retained_count==sizeof(retained)/sizeof(retained[0]))return 125;
            retained[retained_count++]=request.fds[i];
        }
        (void)retained;
        if(received) {if(session)ug_session_note_failure(session,last_sequence[lane],errno);return 125;}
        memset(&response,0,sizeof(response));response.frame=request.frame;
        response.frame.operation|=UG_RESPONSE;response.frame.rights=0;
        response.frame.error=0;memset(response.frame.values,0,sizeof(response.frame.values));
        int result=-1;errno=EPROTO;
        /* These are new BPF/metadata descriptions owned by this response.
         * self_pidfd is borrowed and deliberately excluded. */
        int response_owned[4]={-1,-1,-1,-1};u32 response_owned_count=0;
        if(!incarnation && !lane && request.frame.operation==UG_INIT && request.frame.sequence==1 && request.count==4) {
            if(!request.frame.incarnation ||
               request.frame.values[0]!=bootstrap_deadline_ns ||
               bootstrap_remaining(bootstrap_deadline_ns)<0)return 125;
            incarnation=request.frame.incarnation;parent_pidfd=request.fds[3];
            response.fds[0]=self_pidfd;response.count=response.frame.rights=1;
            result=ug_session_open(request.fds[0],request.fds[1],request.fds[2],incarnation,&session);
            if(!result && bootstrap_remaining(bootstrap_deadline_ns)<0)result=-1;
            if(!result) {
                for(u32 i=1;i<4;i++)response.fds[i]=-1;
                result=ug_session_readers(session,&response.fds[1]);
                for(u32 i=1;i<4;i++)if(response.fds[i]>=0)
                    response_owned[response_owned_count++]=response.fds[i];
                if(!result)response.count=response.frame.rights=4;
            }
        } else if(incarnation==request.frame.incarnation &&
                  request.frame.sequence==last_sequence[lane]+1 && session) {
            switch(request.frame.operation) {
            case UG_CREATOR_RECOVERY:
                if(!lane && !request.count) {
                    response.fds[0]=-1;
                    result=ug_session_creator_recovery(session,&response.fds[0]);
                    if(response.fds[0]>=0) {
                        response.count=response.frame.rights=1;
                        response_owned[response_owned_count++]=response.fds[0];
                    }
                }
                break;
            case UG_CONTROLLER_CHANNEL:
                if(!lane && request.count==1 && !controller_registered) {
                    int kind=0;socklen_t length=sizeof(kind);
                    if(!getsockopt(request.fds[0],SOL_SOCKET,SO_TYPE,&kind,&length) &&
                       kind==SOCK_SEQPACKET) {
                        controller=request.fds[0];controller_registered=true;result=0;
                    }
                }
                break;
            case UG_CONTROLLER_TASK:
                if(!lane && request.count==1 && controller_registered)
                    result=ug_session_bind_controller(session,request.fds[0],request.frame.sequence);
                break;
            case UG_ARM:
                if(!lane && request.count==1 && controller_registered)
                    result=ug_session_arm_creator(session,request.fds[0],request.frame.sequence);
                break;
            case UG_BIRTH: {
                struct ug_birth birth;
                if(!request.count) {
                    result=ug_session_observe_birth(session,request.frame.values[0],&birth);
                    if(!result)memcpy(response.frame.values,&birth,sizeof(birth));
                }
                break;
            }
            case UG_INITIAL:
                if(lane && request.count==1)
                    result=ug_session_register_initial(session,request.fds[0],request.frame.sequence);
                break;
            case UG_PROBE_ARM:
            case UG_PROBE_SUBMIT:
            case UG_PROBE_COMPLETE:
            case UG_PROBE_RETIRE:
                result=probe_request(session,lane,&request,&response);
                break;
            case UG_TERMINAL: {
                if(!lane && !request.count && request.frame.values[0]) {
                    if(!terminal_deadline)terminal_deadline=request.frame.values[0];
                    if(request.frame.values[0]!=terminal_deadline ||
                       bootstrap_remaining(terminal_deadline)<0)break;
                    int inventory=-1;
                    result=ug_session_prepare_terminal(session,request.frame.sequence,&terminal_prepared,&inventory);
                    if(inventory>=0)response_owned[response_owned_count++]=inventory;
                    if(!result) {
                        response.fds[0]=inventory;response.count=response.frame.rights=1;
                        memcpy(response.frame.values,&terminal_prepared,sizeof(terminal_prepared));
                    }
                }
                break;
            }
            case UG_TERMINAL_RELEASE: {
                struct ug_object_close closed;
                if(!lane && !request.count && terminal_prepared.record_ordinal &&
                   request.frame.values[0]==terminal_prepared.record_ordinal) {
                    result=ug_session_close_terminal(session,request.frame.sequence,
                        terminal_prepared.sequence,terminal_prepared.record_ordinal,terminal_deadline,&closed);
                    if(!result)memcpy(response.frame.values,&closed,sizeof(closed));
                }
                break;
            }
            case UG_STOP:
                if(!request.count)result=ug_session_note_failure(session,request.frame.sequence,ECANCELED);
                break;
            default:break;
            }
        }
        if(request.frame.operation==UG_INIT && !result &&
           bootstrap_remaining(bootstrap_deadline_ns)<0)result=-1;
        last_sequence[lane]=request.frame.sequence;
        if(result)response.frame.error=errno?errno:EIO;
        int sent=ug_channel_send(channel,&response),send_error=errno,close_error=0;
        /* SCM duplicates are explicit local owners, not transferred originals.
         * Closing only BPF/immutable metadata descriptions cannot final-fput a
         * guest socket. Pinned policy/object owners survive until the protocol. */
        for(u32 i=0;i<response_owned_count;i++)
            if(close(response_owned[i]) && !close_error)close_error=errno?errno:EIO;
        if(sent || close_error) {
            if(session)ug_session_note_failure(session,last_sequence[lane],sent?send_error:close_error);
            return 125; /* Unknown receipt remains in outside recovery. */
        }
        if(request.frame.operation==UG_TERMINAL_RELEASE && !result)return 0;
        if(request.frame.operation==UG_TERMINAL || request.frame.operation==UG_TERMINAL_RELEASE)
            continue; /* Provisional phase1 is never a final certificate. */
        if(result || request.frame.operation==UG_STOP) {
            if(session)ug_session_note_failure(session,last_sequence[lane],response.frame.error);
            if(!session || request.frame.operation==UG_STOP)return 125;
            /* A verified failed command closes ordinary admission via failed,
             * but the separate parent lane can still request aggregate cleanup.
             * Partial initial/unknown kernel states must pass the SAME proof. */
        }
    }
}
