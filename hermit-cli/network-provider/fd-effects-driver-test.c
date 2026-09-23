/* SPDX-License-Identifier: MIT */
/* Executes the production FD collection/ACK functions with deterministic map
 * failures. No BPF load, kernel hook, TCP socket or privileged operation. */
#include <assert.h>
#include <errno.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include "fd-effects.h"
#define BPF_EXIST 2
struct ap_pending_command {
    struct ap_task_command submitted;
    struct ap_fd_accept fd_receipt;
    bool fd_collected;
};
struct ap_session {
    void *object;
    u64 incarnation;
    bool ready;
    struct ap_pending_command pending[AP_COMMANDS];
};
static struct ap_fd_accept map_receipt;
static struct ap_command_result command_result;
static int completion_error,lookup_error_at,lookups,update_error,collect_calls;
static bool change_second;
static int invalid(void) { errno=EINVAL;return -1; }
static int unavailable(void) { errno=ENODATA;return -1; }
static int enter_commands(struct ap_session *s) { return s && s->ready ? 0 : invalid(); }
static void leave_commands(struct ap_session *s) { (void)s; }
static int bpf_object__find_map_fd_by_name(void *o,const char *name) {
    (void)o;return strcmp(name,"fd_status")==0 ? 2 : 1;
}
static int bpf_map_lookup_elem(int map,const void *key,void *out) {
    (void)key;
    if(map==2) { memset(out,0,sizeof(struct ap_fd_status));return 0; }
    lookups++;
    if(lookups==lookup_error_at) { errno=EIO;return -1; }
    memcpy(out,&map_receipt,sizeof(map_receipt));
    if(change_second && lookups==2)((struct ap_fd_accept *)out)->task++;
    return 0;
}
static int bpf_map_update_elem(int map,const void *key,const void *value,int flags) {
    (void)map;(void)key;(void)flags;
    if(update_error) { errno=EBUSY;return -1; }
    memcpy(&map_receipt,value,sizeof(map_receipt));return 0;
}
static int bpf_map_delete_elem(int map,const void *key) { (void)map;(void)key;return 0; }
static int submit(struct ap_session *s,int pidfd,struct ap_task_command *c) {
    (void)s;(void)pidfd;c->command=7;return 0;
}
static int read_completion(struct ap_session *s,int pidfd,u64 command,u64 op,struct ap_command_result *out) {
    (void)s;(void)pidfd;(void)command;(void)op;*out=command_result;
    if(completion_error) { errno=completion_error;return -1; }
    return 0;
}
static int collect(struct ap_session *s,int pidfd,const struct ap_command_result *r) {
    (void)s;(void)pidfd;(void)r;collect_calls++;return 0;
}
#include "fd-effects-driver.h"
static void reset(struct ap_session *s) {
    memset(s,0,sizeof(*s));s->ready=true;s->incarnation=3;
    completion_error=lookup_error_at=lookups=update_error=collect_calls=0;change_second=false;
    s->pending[7].submitted=(struct ap_task_command){.provider=3,.command=7,
        .operation=AP_ACCEPT_EFFECT,.expected_object=11,.generation_before=19,
        .expected_level=5};
    command_result=(struct ap_command_result){.command=7,.operation=AP_ACCEPT_EFFECT,
        .task=23,.start_boottime=29,.identity={3,31,37},.creation=41,.cookie=43,
        .returned=8,.phase=AP_COMMAND_DONE};
    map_receipt=(struct ap_fd_accept){.command=7,.accept_lease=19,.task=23,
        .task_start=29,.table=47,.file=53,.install_begin=59,.install_end=61,
        .listener={3,11,37},.child={3,31,37},.creation=41,.cookie=43,
        .phases=127,.requested_fd=5,.returned_fd=8};
}
int main(void) {
    struct ap_session s;struct ap_command_result result;struct ap_fd_accept receipt;
    unsigned checks=0;
#define CHECK(v) do { assert(v);checks++; } while(0)
    reset(&s);completion_error=ENODATA;lookup_error_at=1;
    CHECK(ap_collect_accept(&s,9,7,&result,&receipt)==-1);
    CHECK(errno==ENODATA && result.command==7 && collect_calls==0);
    CHECK(!s.pending[7].fd_collected && s.pending[7].submitted.command==7);
    reset(&s);completion_error=EACCES;
    CHECK(ap_collect_accept(&s,9,7,&result,&receipt)==-1);
    CHECK(errno==EACCES && receipt.command==7 && collect_calls==0);
    reset(&s);lookup_error_at=2;
    CHECK(ap_collect_accept(&s,9,7,&result,&receipt)==-1);
    CHECK(errno==EIO && receipt.task==23 && collect_calls==0);
    CHECK(!s.pending[7].fd_collected && s.pending[7].submitted.command==7);
    reset(&s);change_second=true;
    CHECK(ap_collect_accept(&s,9,7,&result,&receipt)==-1);
    CHECK(errno==EPROTO && collect_calls==0 && !s.pending[7].fd_collected);
    reset(&s);
    CHECK(ap_collect_accept(&s,9,7,&result,&receipt)==0);
    CHECK(collect_calls==1 && s.pending[7].fd_collected);
    update_error=1;
    CHECK(fd_ack_accept(&s,&s.pending[7])==-1);
    CHECK(errno==EBUSY && s.pending[7].submitted.command==7 && s.pending[7].fd_collected);
    CHECK(map_receipt.command==7);
    reset(&s);memset(&map_receipt,0,sizeof(map_receipt));
    CHECK(fd_reserve_accept(&s,&s.pending[7].submitted)==0);
    CHECK(map_receipt.command==7 && map_receipt.accept_lease==19);
    printf("fd-effect driver: %u checks\n",checks);
    return 0;
}
