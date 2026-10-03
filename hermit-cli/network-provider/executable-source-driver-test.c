/* SPDX-License-Identifier: BSD-3-Clause */
/* Actual prepare/reserve/double-collect/ACK, controlled command/map effects. */
#define AP_GROUPED_PROVIDER 1
#define AP_FTRACE_PROVIDER 1
#include <assert.h>
#include <errno.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#define main predicate_controls_main
#include "executable-source-test.c"
#undef main
#define BPF_EXIST 2
struct ap_pending_command {struct ap_task_command submitted;struct ap_executable_source executable_receipt;bool executable_collected;};
struct ap_session {bool ready;u64 group_anchor;struct ap_pending_command pending[AP_COMMANDS];};
static struct ap_executable_source cell;
static struct ap_command_result completion;
static int completion_error,lookup_error_at,lookups,update_error,collect_error,collect_calls,submit_calls;
static int submitted_pidfd,collected_pidfd;
static bool changed_second,status_problem,corrupt_clear;
static int invalid(void) {errno=EINVAL;return -1;}
static int unavailable(void) {errno=ENODATA;return -1;}
static int enter_commands(struct ap_session *s) {return s&&s->ready?0:invalid();}
static void leave_commands(struct ap_session *s) {(void)s;}
static int fd_map(struct ap_session *s,const char *name) {(void)s;assert(!strcmp(name,"executable_sources"));return 1;}
static int bpf_map_lookup_elem(int fd,const void *key,void *out) {
    assert(fd==1 && *(const u32 *)key==7);lookups++;
    if(lookups==lookup_error_at) {errno=EIO;return -1;}
    memcpy(out,&cell,sizeof(cell));if(changed_second&&lookups==2)((struct ap_executable_source *)out)->entered.task++;
    return 0;
}
static int bpf_map_update_elem(int fd,const void *key,const void *value,int flags) {
    assert(fd==1 && *(const u32 *)key==7 && flags==BPF_EXIST);
    if(update_error) {errno=EBUSY;return -1;}
    memcpy(&cell,value,sizeof(cell));if(corrupt_clear&&!cell.intent.command)cell.problem=1;return 0;
}
int ap_read_fd_status(struct ap_session *s,struct ap_fd_status *out) {(void)s;*out=(struct ap_fd_status){.problem=status_problem};return 0;}
static int executable_reserve(struct ap_session *,const struct ap_task_command *,const struct ap_executable_intent *);
static int submit_with_executable(struct ap_session *s,int pidfd,struct ap_task_command *c,const struct ap_executable_intent *intent) {
    submit_calls++;submitted_pidfd=pidfd;c->command=7;c->provider=3;
    if(executable_reserve(s,c,intent))return -1;s->pending[7].submitted=*c;return 0;
}
static int read_completion(struct ap_session *s,int pidfd,u64 command,u64 op,struct ap_command_result *out) {
    (void)s;assert(pidfd==17&&command==7&&op==26);*out=completion;
    if(completion_error) {errno=completion_error;return -1;}return 0;
}
static int collect(struct ap_session *s,int pidfd,const struct ap_command_result *r) {
    assert(r->command==7&&s->pending[7].executable_collected);
    assert(!memcmp(&s->pending[7].executable_receipt,&cell,sizeof(cell)));
    collected_pidfd=pidfd;collect_calls++;if(collect_error) {errno=EAGAIN;return -1;}return 0;
}
#include "executable-source-driver.h"
static void reset(struct ap_session *s) {
    memset(s,0,sizeof(*s));s->ready=true;s->group_anchor=AP_GROUPED_CONNECT_IMAGE;
    completion_error=lookup_error_at=lookups=update_error=collect_error=collect_calls=submit_calls=0;
    submitted_pidfd=collected_pidfd=0;changed_second=status_problem=corrupt_clear=false;
    s->pending[7].submitted=command();completion=result();cell=receipt();
}
int main(void) {
    predicate_controls_main();unsigned n=0;struct ap_session s;u64 ticket=0;
    struct ap_command_result r;struct ap_executable_source e;
#undef CHECK
#define CHECK(x) do {assert(x);n++;} while(0)
    reset(&s);memset(&cell,0,sizeof(cell));
    CHECK(!ap_prepare_executable_source(&s,17,13,0,11,0x401040,32,0x700000,0x701000,&ticket));
    CHECK(ticket==7&&submitted_pidfd==17&&submit_calls==1&&cell.intent.owner_mm==0&&cell.intent.command==7);
    CHECK(!memcmp(&s.pending[7].submitted,&(struct ap_task_command){.provider=3,.command=7,.operation=26,.expected_object=11,.generation_before=13,.expected_level=AP_PTRACE_GETREGSET,.expected_option=1,.original_count=32},sizeof(struct ap_task_command)));
    reset(&s);s.group_anchor=0;CHECK(ap_prepare_executable_source(&s,17,13,0,11,0x401040,32,1,2,&ticket)==-1&&errno==ENODATA&&submit_calls==0);
    reset(&s);CHECK(ap_prepare_executable_source(&s,17,13,0,11,0x401ff0,32,1,2,&ticket)==-1&&errno==EINVAL&&submit_calls==0);
    reset(&s);CHECK(executable_reserve(&s,&s.pending[7].submitted,&cell.intent)==-1&&errno==EPROTO);
    reset(&s);CHECK(ap_collect_executable_source(&s,17,7,&r,&e)==0&&collect_calls==1&&collected_pidfd==17);
    CHECK(!memcmp(&e,&cell,sizeof(e))&&s.pending[7].executable_collected);
    CHECK(!executable_ack(&s,&s.pending[7])&&!cell.intent.command&&!cell.problem);
    for(unsigned variant=0;variant<10;variant++) {
        reset(&s);
        switch(variant) {case 0:completion_error=ENODATA;break;case 1:lookup_error_at=1;break;case 2:lookup_error_at=2;break;
        case 3:changed_second=true;break;case 4:status_problem=true;break;case 5:cell.problem=1;break;
        case 6:cell.ptrace_return=-14;completion.returned=-14;break;case 7:cell.phases=3;break;
        case 8:cell.returned.writecount=0;break;case 9:cell.returned.tracer++;break;}
        CHECK(ap_collect_executable_source(&s,17,7,&r,&e)==-1);
        CHECK(!s.pending[7].executable_collected&&collect_calls==0&&cell.intent.command==7);
        CHECK(executable_ack(&s,&s.pending[7])==-1&&cell.intent.command==7);
    }
    reset(&s);collect_error=true;CHECK(ap_collect_executable_source(&s,17,7,&r,&e)==-1&&errno==EAGAIN&&s.pending[7].executable_collected);
    reset(&s);CHECK(!ap_collect_executable_source(&s,17,7,&r,&e));cell.entered.task++;
    CHECK(executable_ack(&s,&s.pending[7])==-1&&errno==EPROTO&&cell.intent.command==7);
    reset(&s);CHECK(!ap_collect_executable_source(&s,17,7,&r,&e));update_error=true;
    CHECK(executable_ack(&s,&s.pending[7])==-1&&errno==EBUSY&&cell.intent.command==7);
    update_error=false;corrupt_clear=true;CHECK(executable_ack(&s,&s.pending[7])==-1&&errno==EPROTO&&cell.problem==1);
    printf("executable actual driver: %u checks\n",n);return 0;
}
