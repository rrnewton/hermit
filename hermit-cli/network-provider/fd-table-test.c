/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#include <assert.h>
#include <stdio.h>
#include "fd-effects.h"
#include "retirement-target.h"
static struct ap_fd_event event(u64 sequence,u64 kind,u64 table,u64 dependency) {
    return (struct ap_fd_event){.sequence=sequence,.kind=kind,.table=table,
        .dependency=dependency,.task=17,.task_start=23,.fd=-1,.complete=1};
}
int main(void) {
    (void)ap_require_retirement_target;
    unsigned checks=0;
#define CHECK(x) do { assert(x);checks++; } while(0)
    struct ap_fd_event pb=event(1,AP_FD_TABLE_PUT_BEGIN,11,0);
    struct ap_fd_event pe=event(3,AP_FD_TABLE_PUT_END,11,1);
    struct ap_fd_event pr=event(2,AP_FD_TABLE_RETIRED,11,1);
    CHECK(ap_fd_put_matches(&pb,NULL,&pe));
    CHECK(!ap_fd_put_matches(&pb,&pr,&pe));
    pe.returned=1;pe.dependency=2;
    CHECK(ap_fd_put_matches(&pb,&pr,&pe));
    CHECK(!ap_fd_put_matches(&pb,NULL,&pe));
#define PUT_BAD(which,field,value) do { struct ap_fd_event b=pb,r=pr,e=pe;which.field=(value);CHECK(!ap_fd_put_matches(&b,&r,&e)); } while(0)
    PUT_BAD(b,kind,AP_FD_COPY_BEGIN);PUT_BAD(b,table,0);PUT_BAD(b,dependency,8);
    PUT_BAD(b,task,0);PUT_BAD(b,task_start,0);PUT_BAD(b,complete,2);
    PUT_BAD(b,file,4);PUT_BAD(b,fd,4);PUT_BAD(b,previous_file,4);PUT_BAD(b,accept_command,4);
    PUT_BAD(r,table,12);PUT_BAD(r,task,18);PUT_BAD(r,task_start,24);PUT_BAD(r,dependency,2);
    PUT_BAD(r,sequence,4);PUT_BAD(r,file,4);PUT_BAD(r,returned,1);PUT_BAD(r,complete,2);
    PUT_BAD(e,table,12);PUT_BAD(e,task,18);PUT_BAD(e,task_start,24);PUT_BAD(e,returned,0);
    PUT_BAD(e,dependency,1);PUT_BAD(e,file,5);PUT_BAD(e,complete,2);
    CHECK(!ap_fd_put_matches(NULL,&pr,&pe));CHECK(!ap_fd_put_matches(&pb,&pr,NULL));
    struct ap_fd_event cb=event(4,AP_FD_COPY_BEGIN,11,0);
    struct ap_fd_event cs[2]={event(5,AP_FD_COPY_SLOT,12,4),event(6,AP_FD_COPY_SLOT,12,4)};
    cs[0].fd=3;cs[0].file=31;cs[0].returned=1;
    cs[1].fd=7;cs[1].file=31; // Same file, distinct actual aliases/CLOEXEC.
    struct ap_fd_event ce=event(7,AP_FD_COPY_END,12,4);ce.fd=64;ce.returned=2;
    CHECK(ap_fd_copy_matches(&cb,cs,2,&ce));
#define COPY_BAD(which,field,value) do { struct ap_fd_event b=cb,s[2]={cs[0],cs[1]},e=ce;which.field=(value);CHECK(!ap_fd_copy_matches(&b,s,2,&e)); } while(0)
    COPY_BAD(b,table,0);COPY_BAD(b,file,1);COPY_BAD(b,dependency,1);
    COPY_BAD(s[0],fd,-1);COPY_BAD(s[0],file,0);COPY_BAD(s[0],returned,2);
    COPY_BAD(s[1],fd,3);COPY_BAD(s[1],fd,64);COPY_BAD(s[1],sequence,5);
    COPY_BAD(s[1],task,18);COPY_BAD(s[1],task_start,24);COPY_BAD(s[1],table,13);
    COPY_BAD(s[1],dependency,3);COPY_BAD(s[1],complete,2);
    COPY_BAD(e,table,11);COPY_BAD(e,table,0);COPY_BAD(e,fd,0);COPY_BAD(e,fd,65);
    COPY_BAD(e,fd,AP_FD_FILES+64);COPY_BAD(e,returned,1);COPY_BAD(e,dependency,3);
    CHECK(!ap_fd_copy_matches(&cb,NULL,2,&ce));CHECK(!ap_fd_copy_matches(&cb,cs,1,&ce));
    CHECK(!ap_fd_copy_matches(&cb,cs,AP_FD_FILES+1,&ce));
    struct ap_fd_event fail=ce;fail.table=0;fail.fd=-1;fail.returned=-12;
    CHECK(ap_fd_copy_matches(&cb,NULL,0,&fail));
    CHECK(!ap_fd_copy_matches(&cb,cs,2,&fail));
    fail.returned=-4096;CHECK(!ap_fd_copy_matches(&cb,NULL,0,&fail));
    fail.returned=-12;fail.table=12;CHECK(!ap_fd_copy_matches(&cb,NULL,0,&fail));
    struct ap_fd_event eb=event(8,AP_FD_EXEC_BEGIN,12,0);
    struct ap_fd_event er[2]={event(9,AP_FD_EXEC_REMOVE,12,8),event(10,AP_FD_EXEC_REMOVE,12,8)};
    er[0].fd=3;er[0].file=31;er[1].fd=7;er[1].file=31;
    struct ap_fd_event ee=event(11,AP_FD_EXEC_END,12,8);ee.returned=2;
    CHECK(ap_fd_exec_matches(&eb,er,2,&ee));
#define EXEC_BAD(which,field,value) do { struct ap_fd_event b=eb,r[2]={er[0],er[1]},e=ee;which.field=(value);CHECK(!ap_fd_exec_matches(&b,r,2,&e)); } while(0)
    EXEC_BAD(b,table,0);EXEC_BAD(b,dependency,1);EXEC_BAD(b,complete,2);
    EXEC_BAD(r[0],fd,-1);EXEC_BAD(r[0],file,0);EXEC_BAD(r[0],returned,1);
    EXEC_BAD(r[1],fd,3);EXEC_BAD(r[1],table,11);EXEC_BAD(r[1],task,18);
    EXEC_BAD(r[1],task_start,24);EXEC_BAD(r[1],sequence,12);EXEC_BAD(r[1],dependency,7);
    EXEC_BAD(e,returned,1);EXEC_BAD(e,table,13);EXEC_BAD(e,file,1);EXEC_BAD(e,complete,2);
    CHECK(!ap_fd_exec_matches(&eb,NULL,2,&ee));CHECK(!ap_fd_exec_matches(&eb,er,1,&ee));
    CHECK(!ap_fd_exec_matches(&eb,er,AP_FD_JOURNAL+1,&ee));
    ee.returned=0;CHECK(ap_fd_exec_matches(&eb,NULL,0,&ee));
    CHECK(ap_exec_close_site(0xffffffff820427f0ULL,0xffffffff8204289bULL));
    CHECK(!ap_exec_close_site(0xffffffff820427f0ULL,0xffffffff82042896ULL));
    CHECK(!ap_exec_close_site(0xffffffff820427f0ULL,0xffffffff820428a0ULL));
    CHECK(!ap_exec_close_site(0,AP_EXEC_CLOSE_RETURN_OFFSET));
    CHECK(!ap_exec_close_site(~0ULL-AP_EXEC_CLOSE_RETURN_OFFSET+1,0));
    printf("fd table receipt controls: %u checks\n",checks);
}
