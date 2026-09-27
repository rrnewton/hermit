/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#ifndef HERMIT_FD_ENROLLMENT_H
#define HERMIT_FD_ENROLLMENT_H
#include "fd-effects.h"
/* Operation5 remains reserved by the unselected Socket primitive. This command
 * shares the existing31 occupied cells; it creates no separate ticket owner. */
#define AP_TABLE_ENROLLMENT 6
#define AP_PTRACE_GETREGSET 0x4204
#define AP_NT_PRSTATUS 1
#define AP_JOBCTL_FROZEN (1ULL<<24)
#define AP_JOBCTL_TRACED (1ULL<<27)
#define AP_TASK_TRACED 8
#define AP_TASK_FROZEN 0x8000
#define AP_ENROLL_NEW_TABLE 1
#define AP_ENROLL_KNOWN_TABLE 2
#define AP_ENROLL_ENTERED 1
#define AP_ENROLL_CENSUS 2
#define AP_ENROLL_RETURNED 4
struct ap_fd_enrollment {
    u64 command, registration, owner_mm, task, task_start, table;
    u64 begin, end, expected_table, phases, problem;
    u32 slots, files, references, mode;
    s32 ptrace_return;
    u32 reserved;
};
/* The actual GETREGSET interval authenticates quiescence: ptrace_check_attach
 * froze this child and waited inactive, including against SIGKILL. An ordinary
 * stopped-state sample alone is insufficient. A new table must have no other
 * users; a known shared table supplies association only, never a fresh census. */
static __attribute__((always_inline)) inline int ap_fd_enrollment_context(
    u32 state,u64 jobctl,u32 ptrace,int tracer_matches,u32 references,int fresh) {
    return (state==AP_TASK_TRACED || state==AP_TASK_FROZEN) &&
        (jobctl&(AP_JOBCTL_FROZEN|AP_JOBCTL_TRACED))==(AP_JOBCTL_FROZEN|AP_JOBCTL_TRACED) &&
        (ptrace&1) && tracer_matches && references && (!fresh || references==1);
}
static __attribute__((always_inline)) inline int ap_fd_enrollment_matches(
    const struct ap_task_command *c,const struct ap_command_result *r,
    const struct ap_fd_enrollment *e) {
    if(!c || !r || !e || c->operation!=AP_TABLE_ENROLLMENT || !c->provider || !c->command ||
       !c->generation_before || c->expected_level!=AP_PTRACE_GETREGSET || c->expected_option!=AP_NT_PRSTATUS ||
       r->command!=c->command || r->operation!=c->operation || r->phase!=AP_COMMAND_DONE ||
       r->identity.provider!=c->provider || r->identity.object || r->identity.namespace || r->creation || r->cookie ||
       !r->task || !r->start_boottime || r->returned>0 || r->returned< -4095 ||
       e->command!=c->command || e->registration!=c->generation_before || e->owner_mm!=c->generation_after ||
       e->task!=r->task || e->task_start!=r->start_boottime || !e->table ||
       e->expected_table!=c->expected_object || !e->begin || e->end<=e->begin ||
       e->phases!=(AP_ENROLL_ENTERED|AP_ENROLL_CENSUS|AP_ENROLL_RETURNED) || e->problem ||
       e->ptrace_return!=r->returned || e->reserved || !e->references)return 0;
    if(e->mode==AP_ENROLL_NEW_TABLE)
        return !e->expected_table && e->references==1 && e->slots && e->slots<=AP_FD_FILES && e->files<=e->slots;
    return e->mode==AP_ENROLL_KNOWN_TABLE && e->expected_table==e->table && !e->slots && !e->files;
}
static __attribute__((always_inline)) inline int ap_fd_enrollment_event(
    const struct ap_fd_enrollment *e,const struct ap_fd_event *row,u64 kind) {
    return e && row && row->kind==kind && row->sequence && row->complete==1 &&
        row->task==e->task && row->task_start==e->task_start && row->table==e->table &&
        !row->previous_file && row->accept_command==e->command &&
        (kind==AP_FD_ENROLL_SLOT ? (ap_fd_profile_valid(row->mode) &&
         ap_fd_device_valid(row->mode,row->device_major,row->device_minor)) :
         (!row->mode && !row->status_flags && !row->device_major && !row->device_minor));
}
static __attribute__((always_inline)) inline int ap_fd_enrollment_census_matches(
    const struct ap_fd_enrollment *e,const struct ap_fd_event *begin,
    const struct ap_fd_event *slots,u32 count,const struct ap_fd_event *end) {
    if(!e || !ap_fd_enrollment_event(e,begin,AP_FD_ENROLL_BEGIN) ||
       !ap_fd_enrollment_event(e,end,AP_FD_ENROLL_END) ||
       begin->sequence!=e->begin || end->sequence!=e->end || e->end<=e->begin ||
       begin->file || begin->fd!=-1 || begin->returned || begin->dependency ||
       end->file || end->dependency!=e->begin || count!=e->files ||
       count>AP_FD_JOURNAL || (count && !slots) || end->returned!=(s32)count)return 0;
    if(e->mode==AP_ENROLL_KNOWN_TABLE)
        return e->expected_table==e->table && !count && !e->slots && end->fd==-1;
    if(e->mode!=AP_ENROLL_NEW_TABLE || e->expected_table || !e->slots ||
       e->slots>AP_FD_FILES || end->fd!=(s32)e->slots || count>e->slots)return 0;
    for(u32 i=0;i<count;i++) {
        const struct ap_fd_event *row=&slots[i];
        if(!ap_fd_enrollment_event(e,row,AP_FD_ENROLL_SLOT) ||
           row->sequence<=begin->sequence || row->sequence>=end->sequence ||
           row->dependency!=begin->sequence || !row->file || row->fd<0 ||
           (u32)row->fd>=e->slots || (row->returned!=0 && row->returned!=1) ||
           (i && (row->fd<=slots[i-1].fd || row->sequence<=slots[i-1].sequence)))return 0;
        for(u32 j=0;j<i;j++)
            if(slots[j].file==row->file && (slots[j].mode!=row->mode ||
               slots[j].status_flags!=row->status_flags || slots[j].device_major!=row->device_major ||
                slots[j].device_minor!=row->device_minor))return 0;
    }
    return 1;
}
#ifndef __BPF__
/* Prepare on the exact held target pidfd; the actual backend tracer then reads
 * its original GETREGSET into the original output buffer. Collection cannot
 * substitute for that native return. Unknown preparation/collection stays owned. */
int ap_prepare_table_enrollment(struct ap_session *,int exact_pidfd,u64 registration,
                                u64 owner_mm,u64 expected_table,u64 *command);
int ap_collect_table_enrollment(struct ap_session *,int exact_pidfd,u64 command,
                                struct ap_command_result *,struct ap_fd_enrollment *);
#endif
#endif
