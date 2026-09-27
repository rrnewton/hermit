/* SPDX-License-Identifier: MIT */
#include "fd-effects.h"
#include <assert.h>
#include <stdio.h>
static unsigned checked;
#define CHECK(x) do { assert(x); checked++; } while(0)
int main(void) {
    struct ap_fd_event b={.sequence=10,.kind=AP_FD_REMOVE_BEGIN,.task=7,.task_start=8,.table=9,.fd=3,.complete=1};
    struct ap_fd_event e={.sequence=12,.kind=AP_FD_REMOVE,.task=7,.task_start=8,.table=9,.file=22,.dependency=10,.fd=3,.complete=1};
    CHECK(ap_fd_remove_matches(&b,&e));
    CHECK(!ap_fd_remove_matches(NULL,&e));
    CHECK(!ap_fd_remove_matches(&b,NULL));
#define BAD(member,value) do { struct ap_fd_event bad=e;bad.member=value;CHECK(!ap_fd_remove_matches(&b,&bad)); } while(0)
    BAD(kind,AP_FD_INSTALL_END); BAD(sequence,10); BAD(sequence,9); BAD(dependency,0);
    BAD(dependency,11); BAD(complete,2); BAD(table,10); BAD(task,8); BAD(task_start,9);
    BAD(fd,4); BAD(file,0); BAD(previous_file,22); BAD(accept_command,1); BAD(returned,-1);
#undef BAD
#define BAD(member,value) do { struct ap_fd_event bad=b;bad.member=value;CHECK(!ap_fd_remove_matches(&bad,&e)); } while(0)
    BAD(kind,AP_FD_REMOVE); BAD(sequence,0); BAD(dependency,1); BAD(complete,2); BAD(table,0);
    BAD(task,0); BAD(task_start,0); BAD(file,22); BAD(previous_file,22); BAD(accept_command,1); BAD(returned,1);
#undef BAD
    struct ap_fd_event no_file=e;no_file.kind=AP_FD_REMOVE_NO_FILE;no_file.file=0;
    CHECK(ap_fd_remove_matches(&b,&no_file));
    no_file.file=22;CHECK(!ap_fd_remove_matches(&b,&no_file));
    /* An intervening install does not change the historical removed identity.
     * This function intentionally grants no current-slot authority. */
    struct ap_fd_event install={.sequence=11,.kind=AP_FD_INSTALL_END,.table=9,.file=23,.fd=3,.complete=1};
    CHECK(ap_fd_remove_matches(&b,&e));
    CHECK(install.table==e.table && install.fd==e.fd && install.file!=e.file);
    CHECK(b.sequence<install.sequence && install.sequence<e.sequence);
    printf("remove_interval_controls=%u passed\n",checked);
    return 0;
}
