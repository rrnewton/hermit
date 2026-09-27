/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
/* Production prepare/collect/ACK with deterministic map/command errors. */
#include <assert.h>
#include <errno.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include "fd-enrollment.h"
#define BPF_EXIST 2
struct ap_pending_command {
    struct ap_task_command submitted;
    struct ap_fd_enrollment enrollment_receipt;
    bool enrollment_collected;
};
struct ap_session { bool ready;struct ap_pending_command pending[AP_COMMANDS]; };
static struct ap_fd_enrollment cell;
static struct ap_command_result completion;
static struct ap_task_command submitted;
static int completion_error,lookup_error_at,lookups,update_error,collect_error,collect_calls;
static int submitted_pidfd,collected_pidfd;
static bool changed_second,status_problem;
static int invalid(void) { errno=EINVAL;return -1; }
static int enter_commands(struct ap_session *s) {return s && s->ready ? 0 : invalid();}
static void leave_commands(struct ap_session *s) {(void)s;}
static int fd_map(struct ap_session *s,const char *name) {(void)s;assert(!strcmp(name,"fd_enrollments"));return 1;}
static int bpf_map_lookup_elem(int fd,const void *key,void *out) {
    assert(fd==1 && *(const u32*)key==7);lookups++;
    if(lookups==lookup_error_at) {errno=EIO;return -1;}
    memcpy(out,&cell,sizeof(cell));if(changed_second && lookups==2)((struct ap_fd_enrollment*)out)->task++;
    return 0;
}
static int bpf_map_update_elem(int fd,const void *key,const void *value,int flags) {
    assert(fd==1 && *(const u32*)key==7 && flags==BPF_EXIST);
    if(update_error) {errno=EBUSY;return -1;}
    memcpy(&cell,value,sizeof(cell));return 0;
}
int ap_read_fd_status(struct ap_session *s,struct ap_fd_status *out) {
    (void)s;*out=(struct ap_fd_status){.problem=status_problem};return 0;
}
static int submit(struct ap_session *s,int pidfd,struct ap_task_command *c) {
    (void)s;submitted_pidfd=pidfd;submitted=*c;c->command=7;return 0;
}
static int read_completion(struct ap_session *s,int pidfd,u64 command,u64 operation,struct ap_command_result *out) {
    (void)s;assert(pidfd==17 && command==7 && operation==6);*out=completion;
    if(completion_error) {errno=completion_error;return -1;}return 0;
}
static int collect(struct ap_session *s,int pidfd,const struct ap_command_result *r) {
    assert(r->command==7 && s->pending[7].enrollment_collected);
    assert(!memcmp(&s->pending[7].enrollment_receipt,&cell,sizeof(cell)));
    collected_pidfd=pidfd;collect_calls++;
    if(collect_error) {errno=EAGAIN;return -1;}return 0;
}
#include "fd-enrollment-driver.h"
static void reset(struct ap_session *s) {
    memset(s,0,sizeof(*s));s->ready=true;
    completion_error=lookup_error_at=lookups=update_error=collect_error=collect_calls=0;
    submitted_pidfd=collected_pidfd=0;changed_second=status_problem=false;
    s->pending[7].submitted=(struct ap_task_command){.provider=3,.command=7,.operation=6,
        .generation_before=11,.generation_after=13,.expected_level=AP_PTRACE_GETREGSET,.expected_option=AP_NT_PRSTATUS};
    completion=(struct ap_command_result){.command=7,.operation=6,.task=19,.start_boottime=23,.identity={.provider=3},.phase=AP_COMMAND_DONE};
    cell=(struct ap_fd_enrollment){.command=7,.registration=11,.owner_mm=13,.task=19,.task_start=23,
        .table=29,.begin=31,.end=37,.phases=7,.slots=64,.files=0,.references=1,.mode=1};
}
int main(void) {
    struct ap_session s;struct ap_command_result result;struct ap_fd_enrollment raw;u64 command=0;unsigned n=0;
#define CHECK(v) do {assert(v);n++;} while(0)
    reset(&s);CHECK(ap_prepare_table_enrollment(&s,17,11,13,29,&command)==0);
    CHECK(command==7 && submitted_pidfd==17 && submitted.operation==6 && submitted.expected_object==29);
    CHECK(submitted.generation_before==11 && submitted.generation_after==13 && submitted.expected_level==AP_PTRACE_GETREGSET && submitted.expected_option==1);
    CHECK(ap_prepare_table_enrollment(&s,17,0,13,0,&command)==-1 && errno==EINVAL);
    reset(&s);completion_error=ENODATA;lookup_error_at=1;
    CHECK(ap_collect_table_enrollment(&s,17,7,&result,&raw)==-1 && errno==ENODATA);
    CHECK(result.command==7 && collect_calls==0 && !s.pending[7].enrollment_collected);
    reset(&s);completion_error=EACCES;
    CHECK(ap_collect_table_enrollment(&s,17,7,&result,&raw)==-1 && errno==EACCES);
    CHECK(raw.command==7 && raw.table==29 && collect_calls==0);
    reset(&s);lookup_error_at=2;
    CHECK(ap_collect_table_enrollment(&s,17,7,&result,&raw)==-1 && errno==EIO);
    CHECK(raw.task==19 && !s.pending[7].enrollment_collected && collect_calls==0);
    reset(&s);changed_second=true;
    CHECK(ap_collect_table_enrollment(&s,17,7,&result,&raw)==-1 && errno==EPROTO);
    CHECK(raw.task==20 && !s.pending[7].enrollment_collected && collect_calls==0);
    reset(&s);status_problem=true;
    CHECK(ap_collect_table_enrollment(&s,17,7,&result,&raw)==-1 && errno==EPROTO);
    CHECK(raw.command==7 && collect_calls==0);
    reset(&s);collect_error=true;
    CHECK(ap_collect_table_enrollment(&s,17,7,&result,&raw)==-1 && errno==EAGAIN);
    CHECK(s.pending[7].enrollment_collected && collect_calls==1 && collected_pidfd==17);
    reset(&s);completion.returned=-EFAULT;cell.ptrace_return=-EFAULT;
    CHECK(ap_collect_table_enrollment(&s,17,7,&result,&raw)==0);
    CHECK(raw.ptrace_return==-EFAULT && result.returned==-EFAULT && s.pending[7].enrollment_collected);
    reset(&s);CHECK(fd_ack_enrollment(&s,&s.pending[7])==-1 && errno==EPROTO);
    CHECK(ap_collect_table_enrollment(&s,17,7,&result,&raw)==0 && collect_calls==1);
    update_error=true;CHECK(fd_ack_enrollment(&s,&s.pending[7])==-1 && errno==EBUSY);
    CHECK(cell.command==7 && s.pending[7].enrollment_collected);
    update_error=false;CHECK(fd_ack_enrollment(&s,&s.pending[7])==0);
    CHECK(cell.command==0 && cell.registration==0);
    reset(&s);CHECK(fd_reserve_enrollment(&s,&s.pending[7].submitted)==-1 && errno==EPROTO);
    memset(&cell,0,sizeof(cell));CHECK(fd_reserve_enrollment(&s,&s.pending[7].submitted)==0);
    CHECK(cell.command==7 && cell.registration==11 && cell.owner_mm==13 && cell.table==0);
    printf("table enrollment driver: %u checks\n",n);return 0;
}
