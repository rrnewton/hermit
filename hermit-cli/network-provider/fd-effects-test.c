/* SPDX-License-Identifier: MIT */
#include <assert.h>
#include <stdio.h>
#include "fd-effects.h"

int main(void) {
    struct ap_task_command c={.provider=3,.command=7,.operation=AP_ACCEPT_EFFECT,
        .expected_object=11,.generation_before=19,.generation_after=0,
        .expected_level=5,.expected_option=0};
    struct ap_command_result r={.command=7,.operation=AP_ACCEPT_EFFECT,.task=23,
        .start_boottime=29,.identity={3,31,37},.creation=41,.cookie=43,
        .returned=8,.phase=AP_COMMAND_DONE};
    struct ap_fd_accept a={.command=7,.accept_lease=19,.owner_mm=0,.task=23,
        .task_start=29,.table=47,.file=53,.install_begin=59,.install_end=61,
        .listener={3,11,37},.child={3,31,37},.creation=41,.cookie=43,
        .phases=127,.requested_fd=5,.flags=0,.returned_fd=8};
    unsigned checks=0;
#define CHECK(v) do { assert(v);checks++; } while(0)
    CHECK(ap_fd_accept_matches(&c,&r,&a));
    /* Actual successful installation is historical. A later REMOVE is a
     * separate journal fact, never permission to rewrite this return to errno. */
    struct ap_fd_event removed={.sequence=60,.kind=AP_FD_REMOVE,.table=47,.fd=8,.file=53,.complete=1};
    CHECK(removed.sequence<a.install_end && removed.file==a.file);
    CHECK(ap_fd_accept_matches(&c,&r,&a));
#define BAD(field,value) do { struct ap_fd_accept b=a;b.field=(value);CHECK(!ap_fd_accept_matches(&c,&r,&b)); } while(0)
    BAD(command,8);BAD(accept_lease,20);BAD(owner_mm,1);BAD(task,24);BAD(task_start,30);
    BAD(table,0);BAD(file,0);BAD(install_begin,0);BAD(install_end,0);
    BAD(listener.object,12);BAD(child.object,32);BAD(child.namespace,38);
    BAD(creation,42);BAD(cookie,44);BAD(phases,63);BAD(phases,255);BAD(problem,AP_FD_IDENTITY);
    BAD(requested_fd,6);BAD(flags,2);BAD(returned_fd,9);BAD(do_accept_errno,14);
    struct ap_fd_accept failure=a;struct ap_command_result error=r;
    failure.phases=AP_FD_ENTERED|AP_FD_LISTENER|AP_FD_DEQUEUED|AP_FD_SYSCALL_RETURNED;
    failure.file=failure.install_begin=failure.install_end=0;
    failure.do_accept_errno=14;error.returned=-14;
    CHECK(ap_fd_accept_matches(&c,&error,&failure));
    failure.phases|=AP_FD_INSTALL_ENTERED;
    CHECK(!ap_fd_accept_matches(&c,&error,&failure));
    failure.phases=AP_FD_ENTERED|AP_FD_SYSCALL_RETURNED;
    failure.child=(struct ap_identity){0};failure.creation=failure.cookie=0;
    failure.do_accept_errno=0;error.identity=(struct ap_identity){.provider=3};
    error.creation=error.cookie=0;error.returned=-9;
    CHECK(ap_fd_accept_matches(&c,&error,&failure));
    error.returned=-4096;
    CHECK(!ap_fd_accept_matches(&c,&error,&failure));
    printf("fd-effect predicates: %u checks\n",checks);
    return 0;
}
