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
/* Exec-created outside helper. Parent and controller have separate actual
 * endpoints and sequence spaces. Only the parent lane can close admissions or
 * release policy after the exact aggregate proof. EOF never supplies proof. */
int ug_keeper_main(void) {
    int parent=fcntl(STDIN_FILENO,F_DUPFD_CLOEXEC,3);if(parent<0)return 125;
    if(close(STDIN_FILENO))return 125;
    int type=0;socklen_t size=sizeof(type);
    if(getsockopt(parent,SOL_SOCKET,SO_TYPE,&type,&size) || type!=SOCK_SEQPACKET)return 125;
    int self_pidfd=(int)syscall(SYS_pidfd_open,syscall(SYS_gettid),1U);
    if(self_pidfd<0)return 125;
    struct ug_session *session=NULL;u64 incarnation=0,last_sequence[2]={0};
    int parent_pidfd=-1,controller=-1;bool controller_registered=false;
    bool outcome_noted=false,controller_closed=false;
    struct ug_monitor_result outcome={0};
    int retained[4*(UG_MAX_INITIAL_TASKS+8)];u32 retained_count=0;
    for(;;) {
        if(session && parent_pidfd>=0 && !outcome_noted) {
            int observed=ug_session_monitor(session,parent_pidfd,&outcome);
            if(observed<0 || outcome.primary) {
                ug_session_note_failure(session,last_sequence[0],ECANCELED);
                outcome_noted=true;
                /* Keep the parent recovery lane and pinned policy alive.
                 * Independent controller status/pidfd monitor owns abort. */
            }
        }
        struct pollfd ready[3]={{parent,POLLIN,0},{controller,POLLIN,0},
                               {parent_pidfd,POLLIN,0}};
        int n=poll(ready,3,50);
        if(n<0 && errno==EINTR)continue;
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
        if(!incarnation && !lane && request.frame.operation==UG_INIT && request.frame.sequence==1 && request.count==4) {
            incarnation=request.frame.incarnation;parent_pidfd=request.fds[3];
            response.fds[0]=self_pidfd;response.count=response.frame.rights=1;
            result=ug_session_open(request.fds[0],request.fds[1],request.fds[2],incarnation,&session);
            if(!result) {
                for(u32 i=1;i<4;i++)response.fds[i]=-1;
                result=ug_session_readers(session,&response.fds[1]);
                if(!result)response.count=response.frame.rights=4;
            }
        } else if(incarnation==request.frame.incarnation &&
                  request.frame.sequence==last_sequence[lane]+1 && session) {
            switch(request.frame.operation) {
            case UG_CREATOR_RECOVERY:
                if(!lane && !request.count) {
                    response.fds[0]=-1;
                    result=ug_session_creator_recovery(session,&response.fds[0]);
                    if(response.fds[0]>=0)response.count=response.frame.rights=1;
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
            case UG_TERMINAL: {
                struct ug_terminal_receipt terminal;
                if(!lane && !request.count) {
                    result=ug_session_terminal(session,request.frame.sequence,&terminal);
                    if(!result)memcpy(response.frame.values,&terminal,sizeof(terminal));
                }
                break;
            }
            case UG_STOP:
                if(!request.count)result=ug_session_note_failure(session,request.frame.sequence,ECANCELED);
                break;
            default:break;
            }
        }
        last_sequence[lane]=request.frame.sequence;
        if(result)response.frame.error=errno?errno:EIO;
        if(ug_channel_send(channel,&response)) {
            if(session)ug_session_note_failure(session,last_sequence[lane],errno);
            return 125; /* Unknown receipt remains in outside recovery. */
        }
        if(request.frame.operation==UG_TERMINAL) {
            if(!result)return 0; /* Links/pins released; map readers remain owned. */
            continue; /* No relaxed proof on busy or unknown terminal state. */
        }
        if(result || request.frame.operation==UG_STOP) {
            if(session)ug_session_note_failure(session,last_sequence[lane],response.frame.error);
            if(!session || request.frame.operation==UG_STOP)return 125;
            /* A verified failed command closes ordinary admission via failed,
             * but the separate parent lane can still request aggregate cleanup.
             * Partial initial/unknown kernel states must pass the SAME proof. */
        }
    }
}
