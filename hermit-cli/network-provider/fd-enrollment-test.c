/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#include <assert.h>
#include <stddef.h>
#include <stdio.h>
#include "fd-enrollment.h"
static struct ap_task_command submitted(void) {
    return (struct ap_task_command){.provider=3,.command=7,.operation=AP_TABLE_ENROLLMENT,
        .generation_before=11,.generation_after=13,.expected_level=AP_PTRACE_GETREGSET,.expected_option=AP_NT_PRSTATUS};
}
static struct ap_command_result result(void) {
    return (struct ap_command_result){.command=7,.operation=AP_TABLE_ENROLLMENT,.task=17,.start_boottime=19,
        .identity={3,0,0},.phase=AP_COMMAND_DONE};
}
static struct ap_fd_enrollment receipt(void) {
    return (struct ap_fd_enrollment){.command=7,.registration=11,.owner_mm=13,.task=17,.task_start=19,
        .table=23,.begin=29,.end=37,.phases=7,.slots=64,.files=2,.references=1,.mode=AP_ENROLL_NEW_TABLE};
}
static struct ap_fd_event row(u64 sequence,u64 kind,int fd,u64 file,int flags) {
    return (struct ap_fd_event){.sequence=sequence,.kind=kind,.task=17,.task_start=19,.table=23,
        .file=file,.dependency=kind==AP_FD_ENROLL_BEGIN?0:29,.accept_command=7,.fd=fd,.returned=flags,.complete=1,.mode=kind==AP_FD_ENROLL_SLOT?010600:0,
        .status_flags=kind==AP_FD_ENROLL_SLOT?1:0};
}
int main(void) {
    unsigned checks=0;
#define CHECK(v) do { assert(v);checks++; } while(0)
    const u64 jobs=AP_JOBCTL_FROZEN|AP_JOBCTL_TRACED;
    CHECK(ap_fd_enrollment_context(8,jobs,1,1,1,1));
    CHECK(ap_fd_enrollment_context(0x8000,jobs,1,1,1,1));
    CHECK(!ap_fd_enrollment_context(0,jobs,1,1,1,1));
    CHECK(!ap_fd_enrollment_context(4,jobs,1,1,1,1));
    CHECK(!ap_fd_enrollment_context(8,jobs^AP_JOBCTL_FROZEN,1,1,1,1));
    CHECK(!ap_fd_enrollment_context(8,jobs^AP_JOBCTL_TRACED,1,1,1,1));
    CHECK(!ap_fd_enrollment_context(8,jobs,0,1,1,1));
    CHECK(!ap_fd_enrollment_context(8,jobs,1,0,1,1));
    CHECK(!ap_fd_enrollment_context(8,jobs,1,1,0,1));
    CHECK(!ap_fd_enrollment_context(8,jobs,1,1,2,1));
    CHECK(ap_fd_enrollment_context(8,jobs,1,1,2,0));
    struct ap_task_command c=submitted();struct ap_command_result r=result();struct ap_fd_enrollment e=receipt();
    CHECK(sizeof(e)==112 && offsetof(struct ap_fd_enrollment,slots)==88);
    CHECK(ap_fd_enrollment_matches(&c,&r,&e));
#define BAD_C(field,value) do { c=submitted();c.field=value;CHECK(!ap_fd_enrollment_matches(&c,&r,&e));c=submitted(); } while(0)
    BAD_C(provider,5);BAD_C(command,38);BAD_C(operation,5);BAD_C(generation_before,12);BAD_C(generation_after,14);
    BAD_C(expected_level,AP_PTRACE_GETREGSET+1);BAD_C(expected_option,2);BAD_C(expected_object,99);
#define BAD_R(field,value) do { r=result();r.field=value;CHECK(!ap_fd_enrollment_matches(&c,&r,&e));r=result(); } while(0)
    BAD_R(command,38);BAD_R(operation,5);BAD_R(phase,AP_COMMAND_RUNNING);BAD_R(identity.provider,4);
    BAD_R(identity.object,1);BAD_R(identity.namespace,1);BAD_R(creation,1);BAD_R(cookie,1);
    BAD_R(task,18);BAD_R(start_boottime,20);BAD_R(returned,1);BAD_R(returned,-4096);
#define BAD_E(field,value) do { e=receipt();e.field=value;CHECK(!ap_fd_enrollment_matches(&c,&r,&e));e=receipt(); } while(0)
    BAD_E(command,38);BAD_E(registration,12);BAD_E(owner_mm,14);BAD_E(task,18);BAD_E(task_start,20);
    BAD_E(table,0);BAD_E(begin,0);BAD_E(end,29);BAD_E(expected_table,23);BAD_E(phases,3);BAD_E(phases,15);
    BAD_E(problem,AP_FD_MISSING);BAD_E(references,2);BAD_E(slots,0);BAD_E(slots,AP_FD_FILES+1);
    BAD_E(files,65);BAD_E(mode,3);BAD_E(ptrace_return,-14);BAD_E(reserved,1);
    e.ptrace_return=-14;r.returned=-14;
    CHECK(ap_fd_enrollment_matches(&c,&r,&e)); /* Known native error is retained, not admitted. */
    CHECK(r.returned!=0);
    e=receipt();r=result();e.expected_table=c.expected_object=23;e.mode=AP_ENROLL_KNOWN_TABLE;
    e.slots=e.files=0;e.references=3;
    CHECK(ap_fd_enrollment_matches(&c,&r,&e));
    e.table=24;CHECK(!ap_fd_enrollment_matches(&c,&r,&e));
    e=receipt();c=submitted();
    struct ap_fd_event begin=row(29,AP_FD_ENROLL_BEGIN,-1,0,0),end=row(37,AP_FD_ENROLL_END,64,0,2);
    struct ap_fd_event slots[2]={row(31,AP_FD_ENROLL_SLOT,0,41,0),row(33,AP_FD_ENROLL_SLOT,7,41,1)};
    CHECK(sizeof(struct ap_fd_event)==112 && offsetof(struct ap_fd_event,complete)==80 &&
          offsetof(struct ap_fd_event,mode)==88 && offsetof(struct ap_fd_event,device_major)==96 &&
          offsetof(struct ap_fd_event,device_minor)==100 && offsetof(struct ap_fd_event,source_ioctl_dispatch)==104);
    CHECK(ap_fd_enrollment_census_matches(&e,&begin,slots,2,&end)); /* exact two aliases */
    CHECK(!ap_fd_enrollment_census_matches(&e,&begin,slots,1,&end));
    CHECK(!ap_fd_enrollment_census_matches(&e,&begin,NULL,2,&end));
#define BAD_ROW(target,field,value) do { struct ap_fd_event saved=target;target.field=value;CHECK(!ap_fd_enrollment_census_matches(&e,&begin,slots,2,&end));target=saved; } while(0)
    BAD_ROW(begin,complete,2);BAD_ROW(begin,task,18);BAD_ROW(begin,task_start,20);BAD_ROW(begin,accept_command,38);
    BAD_ROW(begin,table,24);BAD_ROW(begin,fd,0);BAD_ROW(begin,file,1);BAD_ROW(begin,dependency,1);
    BAD_ROW(end,dependency,28);BAD_ROW(end,sequence,29);BAD_ROW(end,fd,63);BAD_ROW(end,returned,1);
    BAD_ROW(slots[0],file,0);BAD_ROW(slots[0],previous_file,1);BAD_ROW(slots[0],returned,2);
    BAD_ROW(slots[1],fd,0);BAD_ROW(slots[1],fd,64);BAD_ROW(slots[1],sequence,31);BAD_ROW(slots[1],accept_command,38);
    BAD_ROW(begin,mode,010600);BAD_ROW(end,status_flags,1);
    BAD_ROW(slots[0],mode,0);BAD_ROW(slots[0],mode,0200000|010600);
    BAD_ROW(slots[1],mode,0100644);BAD_ROW(slots[1],status_flags,2);
    const u32 modes[]={0000000,0000600,0010600,0020600,0040755,0060600,0100644,0120777,0140600};
    for(unsigned i=0;i<sizeof(modes)/sizeof(modes[0]);i++) {
        slots[0].mode=slots[1].mode=modes[i];
        CHECK(ap_fd_enrollment_census_matches(&e,&begin,slots,2,&end));
    }
    /* kind0 is real anonymous inode occupancy, not a guessed subtype. The
     * original one-alias mode0 negative above still rejects alias disagreement. */
    slots[0].mode=slots[1].mode=0600;
    BAD_ROW(slots[0],mode,0030600);BAD_ROW(slots[0],device_major,1);
    BAD_ROW(begin,device_major,1);BAD_ROW(end,device_minor,1);
    slots[0].mode=slots[1].mode=0020600;
    slots[0].device_major=slots[1].device_major=1;
    slots[0].device_minor=slots[1].device_minor=8;
    CHECK(ap_fd_enrollment_census_matches(&e,&begin,slots,2,&end));
    BAD_ROW(slots[0],device_minor,9);BAD_ROW(slots[0],device_major,0x1000);
    BAD_ROW(slots[0],device_minor,0x100000);
    slots[0].device_minor=slots[1].device_minor=9;
    CHECK(ap_fd_enrollment_census_matches(&e,&begin,slots,2,&end));
    CHECK(ap_fd_device_major((1U<<20)|8)==1 && ap_fd_device_minor((1U<<20)|8)==8);
    CHECK(ap_fd_device_major((1U<<20)|9)==1 && ap_fd_device_minor((1U<<20)|9)==9);
    CHECK(ap_fd_device_major(0xffffffffU)==0xfff && ap_fd_device_minor(0xffffffffU)==0xfffff);
    CHECK(ap_fd_device_major(0x108)==0 && ap_fd_device_minor(0x108)==0x108); /* libc1:8 != kernel1:8 */
    CHECK(ap_fd_device_valid(0060600,0xfff,0xfffff));
    CHECK(!ap_fd_device_valid(0100600,1,8));
    slots[0].device_major=slots[1].device_major=0;
    slots[0].device_minor=slots[1].device_minor=0;
    slots[0].mode=0100644;slots[1].mode=0140600;slots[1].file=42;
    CHECK(ap_fd_enrollment_census_matches(&e,&begin,slots,2,&end)); /* distinct regular/socket */
    slots[0].mode=slots[1].mode=010600;slots[1].file=41;
    /* Identical current numeric/stat profiles never identify a dispatcher.
     * The table address/handler are physical observations, translated through
     * a valid anchor. Exercise relocation as well as the zero-slide image. */
    for(unsigned slide=0;slide<2;slide++) {
        u64 anchor=AP_GROUPED_CONNECT_IMAGE-(u64)slide*0x200000;
        u64 null_ops=ap_grouped_image_address(anchor,AP_SOURCE_NULL_FOPS_IMAGE);
        u64 btrfs_ops=ap_grouped_image_address(anchor,AP_SOURCE_BTRFS_FOPS_IMAGE);
        u64 btrfs_ioctl=ap_grouped_image_address(anchor,AP_SOURCE_BTRFS_IOCTL_IMAGE);
        CHECK(ap_fd_source_ioctl_dispatch(anchor,null_ops,0,0,0,0020600,1,3)==AP_SOURCE_IOCTL_DISPATCH_NULL);
        CHECK(ap_fd_source_ioctl_dispatch(anchor,btrfs_ops,btrfs_ioctl,btrfs_ops,AP_SOURCE_BTRFS_MAGIC,0100600,0,0)==AP_SOURCE_IOCTL_DISPATCH_BTRFS);
        CHECK(!ap_fd_source_ioctl_dispatch(anchor,null_ops+8,0,0,0,0020600,1,3));
        CHECK(!ap_fd_source_ioctl_dispatch(anchor,null_ops,btrfs_ioctl,0,0,0020600,1,3));
        CHECK(!ap_fd_source_ioctl_dispatch(anchor,null_ops,0,0,0,0020600,1,8));
        CHECK(!ap_fd_source_ioctl_dispatch(anchor,btrfs_ops+8,btrfs_ioctl,btrfs_ops+8,AP_SOURCE_BTRFS_MAGIC,0100600,0,0));
        CHECK(!ap_fd_source_ioctl_dispatch(anchor,btrfs_ops,btrfs_ioctl+8,btrfs_ops,AP_SOURCE_BTRFS_MAGIC,0100600,0,0));
        CHECK(!ap_fd_source_ioctl_dispatch(anchor,btrfs_ops,btrfs_ioctl,btrfs_ops+8,AP_SOURCE_BTRFS_MAGIC,0100600,0,0));
        CHECK(!ap_fd_source_ioctl_dispatch(anchor,btrfs_ops,btrfs_ioctl,btrfs_ops,0,0100600,0,0));
        CHECK(!ap_fd_source_ioctl_dispatch(anchor,btrfs_ops,btrfs_ioctl,btrfs_ops,AP_SOURCE_BTRFS_MAGIC,0040600,0,0));
        CHECK(!ap_fd_source_ioctl_dispatch(anchor+1,null_ops,0,0,0,0020600,1,3));
        CHECK(!ap_fd_source_ioctl_dispatch(0,null_ops,0,0,0,0020600,1,3));
    }
    BAD_ROW(begin,source_ioctl_dispatch,AP_SOURCE_IOCTL_DISPATCH_NULL);
    BAD_ROW(end,source_ioctl_dispatch,AP_SOURCE_IOCTL_DISPATCH_BTRFS);
    BAD_ROW(slots[0],source_ioctl_dispatch,3);
    slots[0].mode=slots[1].mode=0020600;
    slots[0].device_major=slots[1].device_major=1;
    slots[0].device_minor=slots[1].device_minor=3;
    slots[0].source_ioctl_dispatch=slots[1].source_ioctl_dispatch=AP_SOURCE_IOCTL_DISPATCH_NULL;
    CHECK(ap_fd_enrollment_census_matches(&e,&begin,slots,2,&end));
    BAD_ROW(slots[0],source_ioctl_dispatch,0); /* aliases cannot split dispatch */
    BAD_ROW(slots[0],source_ioctl_dispatch,AP_SOURCE_IOCTL_DISPATCH_BTRFS);
    slots[0].mode=slots[1].mode=0100600;
    slots[0].device_major=slots[1].device_major=0;
    slots[0].device_minor=slots[1].device_minor=0;
    slots[0].source_ioctl_dispatch=slots[1].source_ioctl_dispatch=AP_SOURCE_IOCTL_DISPATCH_BTRFS;
    CHECK(ap_fd_enrollment_census_matches(&e,&begin,slots,2,&end));
    BAD_ROW(slots[0],source_ioctl_dispatch,0);
    BAD_ROW(slots[0],source_ioctl_dispatch,AP_SOURCE_IOCTL_DISPATCH_NULL);
    e.files=0;end.returned=0;
    CHECK(ap_fd_enrollment_census_matches(&e,&begin,NULL,0,&end)); /* Real complete64-slot empty census */
    end.fd=0;CHECK(!ap_fd_enrollment_census_matches(&e,&begin,NULL,0,&end));
    e.slots=0;e.expected_table=23;e.mode=AP_ENROLL_KNOWN_TABLE;end.fd=-1;
    CHECK(ap_fd_enrollment_census_matches(&e,&begin,NULL,0,&end));
    e.expected_table=0;CHECK(!ap_fd_enrollment_census_matches(&e,&begin,NULL,0,&end));
    printf("table enrollment: %u checks\n",checks);
}
