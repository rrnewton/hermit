/* SPDX-License-Identifier: BSD-3-Clause */
/* Actual prepare/reserve/double-collect/ACK, controlled command/map effects. */
#define AP_GROUPED_PROVIDER 1
#define AP_FTRACE_PROVIDER 1
#define AP_CURRENT_CLOSE_PROFILE_ENABLED 1
#include <assert.h>
#include <errno.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#define main predicate_controls_main
#include "current-close-profile-test.c"
#undef main
#define BPF_EXIST 2
enum {AP_SLOT_COLLECTED=3};
struct ap_pending_command {int state;struct ap_command_result receipt;struct ap_task_command submitted;struct ap_close_profile close_profile_receipt;bool close_profile_collected;};
struct ap_session {bool ready;u64 group_anchor;struct ap_pending_command pending[AP_COMMANDS];};
static struct ap_close_profile cell;
static struct ap_command_result completion;
static int completion_error,lookup_error_at,lookups,update_error,collect_error,collect_calls,submit_calls;
static int submitted_pidfd,collected_pidfd;
static bool changed_second,status_problem,corrupt_clear;
static int invalid(void) {errno=EINVAL;return -1;}
static int unavailable(void) {errno=ENODATA;return -1;}
static int enter_commands(struct ap_session *s) {return s&&s->ready?0:invalid();}
static void leave_commands(struct ap_session *s) {(void)s;}
static int fd_map(struct ap_session *s,const char *name) {(void)s;assert(!strcmp(name,"current_close_profiles"));return 1;}
static int bpf_map_lookup_elem(int fd,const void *key,void *out) {
    assert(fd==1 && *(const u32 *)key==7);lookups++;
    if(lookups==lookup_error_at) {errno=EIO;return -1;}
    memcpy(out,&cell,sizeof(cell));if(changed_second&&lookups==2)((struct ap_close_profile *)out)->entered.task++;
    return 0;
}
static int bpf_map_update_elem(int fd,const void *key,const void *value,int flags) {
    assert(fd==1 && *(const u32 *)key==7 && flags==BPF_EXIST);
    if(update_error) {errno=EBUSY;return -1;}
    memcpy(&cell,value,sizeof(cell));if(corrupt_clear&&!cell.intent.command)cell.problem=1;return 0;
}
int ap_read_fd_status(struct ap_session *s,struct ap_fd_status *out) {(void)s;*out=(struct ap_fd_status){.problem=status_problem};return 0;}
static int close_profile_reserve(struct ap_session *,const struct ap_task_command *,const struct ap_close_profile_intent *);
struct ap_executable_intent;
static int submit_with_intents(struct ap_session *s,int pidfd,struct ap_task_command *c,const struct ap_executable_intent *unused,const struct ap_close_profile_intent *intent) {
    assert(!unused);
    submit_calls++;submitted_pidfd=pidfd;c->command=7;c->provider=3;
    if(close_profile_reserve(s,c,intent))return -1;s->pending[7].submitted=*c;return 0;
}
static int read_completion(struct ap_session *s,int pidfd,u64 command,u64 op,struct ap_command_result *out) {
    (void)s;assert(pidfd==17&&command==7&&op==27);*out=completion;
    if(completion_error) {errno=completion_error;return -1;}return 0;
}
static int collect(struct ap_session *s,int pidfd,const struct ap_command_result *r) {
    assert(r->command==7&&s->pending[7].close_profile_collected);
    assert(!memcmp(&s->pending[7].close_profile_receipt,&cell,sizeof(cell)));
    collected_pidfd=pidfd;collect_calls++;if(collect_error) {errno=EAGAIN;return -1;}s->pending[7].state=AP_SLOT_COLLECTED;s->pending[7].receipt=*r;return 0;
}
#include "current-close-profile-driver.h"
static void reset(struct ap_session *s) {
    memset(s,0,sizeof(*s));s->ready=true;s->group_anchor=AP_GROUPED_CONNECT_IMAGE;
    completion_error=lookup_error_at=lookups=update_error=collect_error=collect_calls=submit_calls=0;
    submitted_pidfd=collected_pidfd=0;changed_second=status_problem=corrupt_clear=false;
    s->pending[7].submitted=command();completion=result();cell=receipt();
}
int main(void) {
    predicate_controls_main();unsigned n=0;struct ap_session s;u64 ticket=0;
    struct ap_command_result r;struct ap_close_profile e;
#undef CHECK
#define CHECK(x) do {assert(x);n++;} while(0)
    reset(&s);struct ap_close_profile_intent intent=cell.intent;intent.command=0;memset(&cell,0,sizeof(cell));
    CHECK(!ap_prepare_current_close_profile(&s,17,&intent,&ticket));
    CHECK(ticket==7&&submitted_pidfd==17&&submit_calls==1&&cell.intent.owner_mm==0&&cell.intent.normal_epoch==0);
    reset(&s);s.group_anchor=0;CHECK(ap_prepare_current_close_profile(&s,17,&intent,&ticket)==-1&&errno==ENODATA&&submit_calls==0);
    reset(&s);CHECK(close_profile_reserve(&s,&s.pending[7].submitted,&cell.intent)==-1&&errno==EPROTO);
    reset(&s);CHECK(!ap_collect_current_close_profile(&s,17,7,&r,&e)&&collect_calls==1&&collected_pidfd==17);
    CHECK(!ap_validate_current_close_profile(&s,&r,&e));
    e.intent.normal_epoch=1;CHECK(ap_validate_current_close_profile(&s,&r,&e)==-1&&errno==ESTALE);e=cell;
    CHECK(!close_profile_ack(&s,&s.pending[7])&&!cell.intent.command&&!cell.problem);
    reset(&s);cell.problem=AP_CLOSE_UNSUPPORTED;cell.entered.linger=cell.returned.linger=1;
    CHECK(!ap_collect_current_close_profile(&s,17,7,&r,&e));
    CHECK(ap_validate_current_close_profile(&s,&r,&e)==-1&&errno==EPROTO);
    CHECK(!close_profile_ack(&s,&s.pending[7])&&!cell.intent.command);
    for(unsigned variant=0;variant<10;variant++) {
        reset(&s);
        switch(variant) {case 0:completion_error=ENODATA;break;case 1:lookup_error_at=1;break;case 2:lookup_error_at=2;break;
        case 3:changed_second=true;break;case 4:status_problem=true;break;case 5:cell.problem=AP_CLOSE_CONTEXT;break;
        case 6:cell.ptrace_return=-14;completion.returned=-14;break;case 7:cell.phases=3;break;
        case 8:cell.returned.raw_file++;break;case 9:cell.returned.tracer++;break;}
        CHECK(ap_collect_current_close_profile(&s,17,7,&r,&e)==-1);
        CHECK(!s.pending[7].close_profile_collected&&collect_calls==0&&cell.intent.command==7);
        CHECK(close_profile_ack(&s,&s.pending[7])==-1&&cell.intent.command==7);
    }
    reset(&s);collect_error=true;CHECK(ap_collect_current_close_profile(&s,17,7,&r,&e)==-1&&errno==EAGAIN&&s.pending[7].close_profile_collected);
    CHECK(ap_validate_current_close_profile(&s,&r,&e)==-1&&errno==ESTALE);
    reset(&s);CHECK(!ap_collect_current_close_profile(&s,17,7,&r,&e));cell.entered.task++;
    CHECK(close_profile_ack(&s,&s.pending[7])==-1&&errno==EPROTO&&cell.intent.command==7);
    reset(&s);CHECK(!ap_collect_current_close_profile(&s,17,7,&r,&e));update_error=true;
    CHECK(close_profile_ack(&s,&s.pending[7])==-1&&errno==EBUSY&&cell.intent.command==7);
    update_error=false;corrupt_clear=true;CHECK(close_profile_ack(&s,&s.pending[7])==-1&&errno==EPROTO&&cell.problem==1);
    printf("current Close actual driver: %u checks\n",n);return 0;
}
