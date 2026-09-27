/* SPDX-License-Identifier: MIT */
/* Executes the production FD collection/ACK functions with deterministic map
 * failures. No BPF load, kernel hook, TCP socket or privileged operation. */
#include <assert.h>
#include <errno.h>
#include <poll.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "fd-effects.h"
#include "task-disarm.h"
#define BPF_EXIST 2
enum ap_slot_state { AP_SLOT_FREE, AP_SLOT_RESERVED, AP_SLOT_ACTIVE,
    AP_SLOT_DISARMING, AP_SLOT_COLLECTED, AP_SLOT_QUARANTINED };
struct ap_pending_command {
    struct ap_stream_copy_owned stream_copy;
    enum ap_slot_state state;
    struct ap_task_command submitted;
    struct ap_command_result receipt;
    struct ap_task_disarm_outcome disarm;
    struct ap_fd_accept fd_receipt;
    bool fd_collected;
    struct ap_original_selection original_selection;
    struct ap_fd_call original_receipt;
    bool original_selected,original_collected,epoll_collected;
    struct ap_native_birth birth_receipt;
    struct ap_fd_call birth_completed;
    bool birth_observed,birth_child_admitted,birth_child_terminal,birth_collected;
};
struct ap_session {
    void *object;
    u64 incarnation;
    bool ready;
    int tasks,commands;
    struct ap_pending_command pending[AP_COMMANDS];
};
static struct ap_fd_accept map_receipt;
static struct ap_command_result command_result;
static int completion_error,lookup_error_at,lookups,update_error,collect_calls;
static bool change_second;
static int observer_error;
static struct ap_fd_call original_call;
static struct ap_task_command original_task;
static int original_lookups,original_delete_error;
static bool original_absent,original_torn;
static unsigned original_torn_component;
static bool child_marker_absent;
static struct ap_task_command child_marker;
static struct ap_task_command last_submitted;
static unsigned submission_count;
static bool task_absent,terminal_ready;
static int terminal_poll_error;
static int fake_poll(struct pollfd *p,nfds_t n,int timeout) {
    assert(n==1 && timeout==0 && p->events==POLLIN);
    if(terminal_poll_error) {errno=terminal_poll_error;return -1;}
    p->revents=terminal_ready && p->fd==9?POLLIN:0;return p->revents?1:0;
}
#define poll fake_poll
static int fd_accept_observer_ready(struct ap_session *s) {
    (void)s;if(observer_error) { errno=observer_error;return -1; }return 0;
}
static int fd_accept_observer_ready_runtime(struct ap_session *s) {
    return fd_accept_observer_ready(s);
}
static int invalid(void) { errno=EINVAL;return -1; }
static int unavailable(void) { errno=ENODATA;return -1; }
static int enter_commands(struct ap_session *s) { return s && s->ready ? 0 : invalid(); }
static void leave_commands(struct ap_session *s) { (void)s; }
static int bpf_object__find_map_fd_by_name(void *o,const char *name) {
    (void)o;return strcmp(name,"fd_status")==0 ? 2 : strcmp(name,"fd_calls")==0 ? 5 : 1;
}
static int bpf_map_lookup_elem(int map,const void *key,void *out) {
    (void)key;
    if(map==2) { memset(out,0,sizeof(struct ap_fd_status));return 0; }
    if(map==3) {
        if(*(const int *)key==12) {
            if(child_marker_absent) {errno=ENOENT;return -1;}
            memcpy(out,&child_marker,sizeof(child_marker));return 0;
        }
        if(task_absent){errno=ESRCH;return -1;}
        memcpy(out,&original_task,sizeof(original_task));return 0;
    }
    if(map==4) { memcpy(out,&command_result,sizeof(command_result));return 0; }
    if(map==5) {
        if(original_absent) { errno=ENOENT;return -1; }
        original_lookups++;memcpy(out,&original_call,sizeof(original_call));
        if(original_lookups==2 && original_torn_component==1)
            ((struct ap_fd_call *)out)->original.selection.table++;
        if(original_lookups==2 && original_torn_component==2)
            ((struct ap_fd_call *)out)->original.selection.file++;
        if(original_lookups==2 && original_torn_component==3)
            ((struct ap_fd_call *)out)->original.epoll_ctl.target_file++;
        if(original_lookups==2 && original_torn_component==4)
            ((struct ap_fd_call *)out)->original.epoll_ctl.event[7]++;
        if(original_torn && original_lookups==2)
            ((struct ap_fd_call *)out)->original.selection.task++;
        return 0;
    }
    lookups++;
    if(lookups==lookup_error_at) { errno=EIO;return -1; }
    memcpy(out,&map_receipt,sizeof(map_receipt));
    if(change_second && lookups==2)((struct ap_fd_accept *)out)->task++;
    return 0;
}
static int bpf_map_update_elem(int map,const void *key,const void *value,int flags) {
    (void)key;(void)flags;
    if(update_error) { errno=EBUSY;return -1; }
    if(map==3) { memcpy(&original_task,value,sizeof(original_task));return 0; }
    if(map==4) { memcpy(&command_result,value,sizeof(command_result));return 0; }
    memcpy(&map_receipt,value,sizeof(map_receipt));return 0;
}
static int bpf_map_delete_elem(int map,const void *key) {
    (void)key;
    if(map==3 && *(const int *)key==12) {child_marker_absent=true;return 0;}
    if(map==3) {task_absent=true;return 0;}
    if(map==5) {
        if(original_delete_error) { errno=original_delete_error;return -1; }
        original_absent=true;
    }
    return 0;
}
int ap_read_status(struct ap_session *s,struct ap_status *status) {
    (void)s;memset(status,0,sizeof(*status));return 0;
}
static int submit(struct ap_session *s,int pidfd,struct ap_task_command *c) {
    (void)s;(void)pidfd;c->command=7;last_submitted=*c;submission_count++;return 0;
}
static int read_completion(struct ap_session *s,int pidfd,u64 command,u64 op,struct ap_command_result *out) {
    (void)s;(void)pidfd;(void)command;(void)op;*out=command_result;
    if(completion_error) { errno=completion_error;return -1; }
    return 0;
}
static int read_command_completion(struct ap_session *s,u64 command,u64 op,struct ap_command_result *out) {
    return read_completion(s,9,command,op,out);
}
static int collect(struct ap_session *s,int pidfd,const struct ap_command_result *r) {
    (void)pidfd;
    if(ap_original_operation(r->operation) || r->operation==AP_NATIVE_BIRTH) {
        s->pending[ap_command_slot(r->command)].receipt=*r;
        s->pending[ap_command_slot(r->command)].state=AP_SLOT_COLLECTED;
    }
    collect_calls++;return 0;
}
static int disarm_task(struct ap_session *s,int pidfd,struct ap_pending_command *p) {
    (void)pidfd;p->state=AP_SLOT_DISARMING;
    if(update_error) { p->state=AP_SLOT_QUARANTINED;errno=EBUSY;return -1; }
    original_task=(struct ap_task_command){.provider=s->incarnation};return 0;
}
/* Existing component harness still substitutes the driver's primitive. The
 * separate production-driver harness owns real update/readback ordering. */
static int disarm_task_observed(struct ap_session *s,int pidfd,struct ap_pending_command *p) {
    return disarm_task(s,pidfd,p);
}
static int quarantine(struct ap_pending_command *p) {p->state=AP_SLOT_QUARANTINED;return -1;}
/* Ring consumption is a libbpf boundary in this map-only harness. The
 * production ring owner has its own focused callback controls. */
static int stream_copy_terminal_ready(struct ap_session *s,struct ap_pending_command *p) {
    (void)s;(void)p;return 1; /* Ring boundary has separate production-consumer controls. */
}
static int stream_copy_drain(struct ap_session *s) {(void)s;return 0;}
static int stream_copy_observer_ready(struct ap_session *s) {(void)s;return 0;}
#include "fd-effects-driver.h"
static void reset(struct ap_session *s) {
    memset(s,0,sizeof(*s));s->ready=true;s->incarnation=3;
    completion_error=lookup_error_at=lookups=update_error=collect_calls=0;change_second=false;
    observer_error=0;child_marker_absent=false;child_marker=(struct ap_task_command){0};
    original_lookups=original_delete_error=0;original_absent=original_torn=false;
    original_torn_component=0;
    task_absent=terminal_ready=false;terminal_poll_error=0;
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
static void original_reset(struct ap_session *s) {
    reset(s);s->tasks=3;s->commands=4;
    struct ap_pending_command *p=&s->pending[7];
    p->state=AP_SLOT_ACTIVE;
    p->submitted=(struct ap_task_command){.provider=3,.command=7,.operation=AP_ORIGINAL_CONNECT,
        .expected_object=19,.generation_before=0x2000,.generation_after=2,
        .expected_level=5,.expected_option=4};
    original_task=p->submitted;
    command_result=(struct ap_command_result){.command=7,.operation=AP_ORIGINAL_CONNECT,
        .task=23,.start_boottime=29,.identity={.provider=3},.phase=AP_COMMAND_RUNNING};
    original_call=(struct ap_fd_call){.command=7,.operation=AP_ORIGINAL_CONNECT,
        .raw_table=0x3000,.function_ip=0x4000,.selected_file=53,
        .selection={.entered=1,.returned=1,.word=0x5001},
        .original={.selection={.command=7,.call=19,.owner_mm=2,.provider=3,
            .task=23,.task_start=29,.table=47,.file=53,.user_address=0x2000,
            .fdput_flags=1,.ready=1,.requested_fd=5,.address_length=4}}};
}
static void original_return(void) {
    command_result.phase=AP_COMMAND_DONE;command_result.returned=-111;
    original_call.original.copy_entered=1;original_call.original.copy_returned=1;
    original_call.original.security_entered=1;original_call.original.security_returned=1;
    original_call.original.address[0]=2;original_call.original.complete=1;
    original_call.original.returned=-111;
}
static void birth_reset(struct ap_session *s) {
    reset(s);s->tasks=3;s->commands=4;s->pending[7].state=AP_SLOT_ACTIVE;
    s->pending[7].submitted=(struct ap_task_command){.provider=3,.command=7,.operation=AP_NATIVE_BIRTH,
        .expected_object=17,.generation_before=47,.generation_after=23,.expected_level=435};
    original_task=s->pending[7].submitted;child_marker=original_task;
    original_call=(struct ap_fd_call){.command=7,.operation=AP_NATIVE_BIRTH};
    original_call.birth=(struct ap_native_birth){.command=7,.call=17,.owner_mm=23,.provider=3,
        .creator_task=(11ULL<<32)|11,.creator_start=29,.creator_table=47,
        .child_task=(12ULL<<32)|12,.child_start=31,.child_table=53,
        .parent_task=(11ULL<<32)|11,.parent_start=29,.copy_begin=59,.copy_end=61,
        .ready=1,.exit_signal=17,.requested_exit_signal=17,.pidfd_fd=-1};
    command_result=(struct ap_command_result){.command=7,.operation=AP_NATIVE_BIRTH,
        .task=original_call.birth.creator_task,.start_boottime=29,.identity={.provider=3},
        .returned=12,.phase=AP_COMMAND_RUNNING};
}

/* Journal state has no fdget premise. Unrelated global fdget miss counters
 * cannot disqualify a direct read of this provider-owned problem bitmap. */
static void journal_observer_controls(void) {
    struct ap_session s;struct ap_fd_status status;
    reset(&s);assert(ap_read_fd_status(&s,&status)==0);
    const int errors[]={ENODATA,EIO};
    for(unsigned i=0;i<2;i++) {
        reset(&s);observer_error=errors[i];errno=0;
        assert(ap_read_fd_status(&s,&status)==0);
        assert(status.problem==0);
    }
    puts("FD journal independent-observer controls=5");
}

/* Same maintained production driver/header, with the existing map backend.
 * These are control inputs, not native kernel effect claims. */
static void close_reset(struct ap_session *s) {
    original_reset(s);
    s->pending[7].submitted.operation=AP_ORIGINAL_CLOSE;
    s->pending[7].submitted.generation_before=0;
    s->pending[7].submitted.expected_option=0;
    original_task=s->pending[7].submitted;
    command_result.operation=AP_ORIGINAL_CLOSE;
    original_call.operation=AP_ORIGINAL_CLOSE;
    original_call.original.selection.user_address=0;
    original_call.original.selection.address_length=0;
    original_call.original.selection.fdput_flags=0;
}
static void original_close_controls(void) {
    unsigned checks=0;
#define CLOSE_CHECK(v) do {assert(v);checks++;} while(0)
    struct ap_session s;struct ap_original_selection selected;
    struct ap_command_result result;struct ap_original_result final;
    u64 command=0;
    close_reset(&s);
    CLOSE_CHECK(ap_prepare_original_close(&s,9,19,2,5,&command)==0);
    CLOSE_CHECK(command==7 && last_submitted.operation==AP_ORIGINAL_CLOSE);
    CLOSE_CHECK(last_submitted.expected_object==19 && last_submitted.generation_after==2);
    CLOSE_CHECK(!last_submitted.generation_before && !last_submitted.expected_option && last_submitted.expected_level==5);
    close_reset(&s);
    CLOSE_CHECK(ap_read_original_selection(&s,9,7,&selected)==0);
    CLOSE_CHECK(selected.file==53 && selected.fdput_flags==0 && selected.ready==1);
    CLOSE_CHECK(!s.pending[7].original_collected && collect_calls==0);
    CLOSE_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==-1 && errno==ENODATA);
    CLOSE_CHECK(!s.pending[7].original_collected && collect_calls==0);
    command_result.phase=AP_COMMAND_DONE;command_result.returned=-EINTR;
    original_call.original.complete=1;original_call.original.returned=-EINTR;
    CLOSE_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==0);
    CLOSE_CHECK(result.returned==-EINTR && final.returned==-EINTR && final.selection.file==53);
    CLOSE_CHECK(s.pending[7].original_collected && collect_calls==1);
    CLOSE_CHECK(fd_ack_original(&s,&s.pending[7])==0 && original_absent);
    /* Actual NO_FILE is distinct from an errno and cannot become success. */
    close_reset(&s);original_call.original.selection.file=0;
    CLOSE_CHECK(ap_read_original_selection(&s,9,7,&selected)==0 && selected.file==0);
    command_result.phase=AP_COMMAND_DONE;original_call.original.complete=1;
    CLOSE_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==-1 && errno==ENODATA);
    CLOSE_CHECK(!s.pending[7].original_collected && collect_calls==0);
    command_result.returned=-EBADF;original_call.original.returned=-EBADF;
    CLOSE_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==0);
    CLOSE_CHECK(fd_ack_original(&s,&s.pending[7])==0 && original_absent);
    /* Connect-only fields cannot be repurposed into Close authority. */
    for(unsigned bad=0;bad<5;bad++) {
        close_reset(&s);
        if(bad==0)original_call.original.selection.user_address=1;
        if(bad==1)original_call.original.selection.address_length=1;
        if(bad==2)original_call.original.selection.fdput_flags=1;
        if(bad==3)original_call.original.selection.command++;
        if(bad==4)original_call.original.selection.ready=0;
        CLOSE_CHECK(ap_read_original_selection(&s,9,7,&selected)==-1);
        CLOSE_CHECK(!s.pending[7].original_selected && collect_calls==0);
    }
    /* Positive but wrong actor/provider IDs are compared at the C owner,
     * where the target task-storage command and result own their domains. */
    for(unsigned bad=0;bad<8;bad++) {
        close_reset(&s);
        if(bad==0)original_call.original.selection.provider++;
        if(bad==1)original_call.original.selection.task++;
        if(bad==2)original_call.original.selection.task_start++;
        if(bad==3)original_task.command++;
        if(bad==4)command_result.command++;
        if(bad==5)original_torn_component=1;
        if(bad==6)original_torn_component=2;
        if(bad==7)command_result.identity.provider++;
        CLOSE_CHECK(ap_read_original_selection(&s,9,7,&selected)==-1);
        CLOSE_CHECK(!s.pending[7].original_selected && collect_calls==0);
    }
    for(unsigned changed=0;changed<2;changed++) {
        close_reset(&s);
        CLOSE_CHECK(ap_read_original_selection(&s,9,7,&selected)==0);
        struct ap_original_selection retained=s.pending[7].original_selection;
        if(changed)original_call.original.selection.file++;
        else original_call.original.selection.table++;
        CLOSE_CHECK(ap_read_original_selection(&s,9,7,&selected)==-1 && errno==ESTALE);
        CLOSE_CHECK(!memcmp(&retained,&s.pending[7].original_selection,sizeof(retained)) && collect_calls==0);
    }
    close_reset(&s);terminal_ready=true;
    struct ap_original_terminal terminal;
    CLOSE_CHECK(ap_retire_dead_original(&s,9,7,&terminal)==0);
    CLOSE_CHECK(terminal.fd_call_present==1 && terminal.original.selection.file==53 && terminal.original.selection.ready==1);
    CLOSE_CHECK(!terminal.original.complete && terminal.task_absent==1 && original_absent);
    assert(checks==53);printf("original close shared driver: %u checks\n",checks);
#undef CLOSE_CHECK
}

static void original_file_reset(struct ap_session *s) {
    original_reset(s);s->pending[7].submitted.operation=AP_ORIGINAL_FILE;
    s->pending[7].submitted.generation_before=72;s->pending[7].submitted.expected_option=3;
    original_task=s->pending[7].submitted;command_result.operation=AP_ORIGINAL_FILE;
    original_call.operation=AP_ORIGINAL_FILE;original_call.original.selection.user_address=72;
    original_call.original.selection.address_length=3;
}
static void original_file_driver_controls(void) {
    unsigned checks=0;
#define FILE_DRIVER_CHECK(x) do {assert(x);checks++;} while(0)
    struct ap_session s;struct ap_original_selection selected;struct ap_command_result result;
    struct ap_original_result final;u64 command=0;
    original_file_reset(&s);
    FILE_DRIVER_CHECK(ap_prepare_original_file(&s,9,19,2,5,72,3,&command)==0);
    FILE_DRIVER_CHECK(command==7 && last_submitted.operation==AP_ORIGINAL_FILE && last_submitted.expected_object==19);
    FILE_DRIVER_CHECK(last_submitted.generation_before==72 && last_submitted.expected_option==3 && last_submitted.expected_level==5 && last_submitted.generation_after==2);
    unsigned submitted=submission_count;
    FILE_DRIVER_CHECK(ap_prepare_original_file(&s,9,19,2,5,71,3,&command)==-1 && errno==EINVAL);
    FILE_DRIVER_CHECK(ap_prepare_original_file(&s,9,19,2,5,72,4,&command)==-1 && errno==EINVAL);
    FILE_DRIVER_CHECK(submission_count==submitted);
    FILE_DRIVER_CHECK(ap_read_original_selection(&s,9,7,&selected)==0 && selected.file==53);
    FILE_DRIVER_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==-1 && errno==ENODATA);
    FILE_DRIVER_CHECK(!s.pending[7].original_collected && collect_calls==0);
    command_result.phase=AP_COMMAND_DONE;command_result.returned=2048;
    original_call.original.complete=1;original_call.original.returned=2048;
    FILE_DRIVER_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==0 && final.returned==2048);
    FILE_DRIVER_CHECK(fd_ack_original(&s,&s.pending[7])==0 && original_absent);
    original_file_reset(&s);original_call.original.selection.file=0;original_call.original.selection.fdput_flags=0;
    FILE_DRIVER_CHECK(ap_read_original_selection(&s,9,7,&selected)==0 && !selected.file);
    command_result.phase=AP_COMMAND_DONE;original_call.original.complete=1;
    FILE_DRIVER_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==-1 && errno==ENODATA);
    command_result.returned=-EBADF;original_call.original.returned=-EBADF;
    FILE_DRIVER_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==0);
    FILE_DRIVER_CHECK(fd_ack_original(&s,&s.pending[7])==0 && original_absent);
    for(unsigned bad=0;bad<9;bad++) {
        original_file_reset(&s);
        if(bad==0)original_call.original.selection.ready=0;if(bad==1)original_call.original.selection.user_address=71;
        if(bad==2)original_call.original.selection.address_length=4;if(bad==3)original_call.original.selection.owner_mm++;
        if(bad==4)original_call.original.selection.provider++;if(bad==5)original_task.command++;
        if(bad==6)command_result.identity.provider++;if(bad==7)original_torn_component=1;if(bad==8)original_torn_component=2;
        FILE_DRIVER_CHECK(ap_read_original_selection(&s,9,7,&selected)==-1);
        FILE_DRIVER_CHECK(!s.pending[7].original_selected && !s.pending[7].original_collected && collect_calls==0);
    }
    original_file_reset(&s);terminal_ready=true;struct ap_original_terminal terminal;
    FILE_DRIVER_CHECK(ap_retire_dead_original(&s,9,7,&terminal)==0);
    FILE_DRIVER_CHECK(terminal.fd_call_present && terminal.original.selection.file==53 && !terminal.original.complete);
    FILE_DRIVER_CHECK(terminal.task_absent && original_absent);
    assert(checks==36);printf("original file common driver: %u checks\n",checks);
#undef FILE_DRIVER_CHECK
}

static void auxiliary_file_reset(struct ap_session *s) {
    original_reset(s);s->pending[7].submitted.operation=AP_AUXILIARY_FILE;
    s->pending[7].submitted.generation_before=72;s->pending[7].submitted.expected_option=3;
    original_task=s->pending[7].submitted;command_result.operation=AP_AUXILIARY_FILE;
    original_call.operation=AP_AUXILIARY_FILE;original_call.original.selection.user_address=72;
    original_call.original.selection.address_length=3;
}
static void auxiliary_file_driver_controls(void) {
    unsigned checks=0;
#define FILE_DRIVER_CHECK(x) do {assert(x);checks++;} while(0)
    struct ap_session s;struct ap_original_selection selected;struct ap_command_result result;
    struct ap_original_result final;u64 command=0;
    auxiliary_file_reset(&s);
    FILE_DRIVER_CHECK(ap_prepare_auxiliary_file(&s,9,19,2,5,&command)==0);
    FILE_DRIVER_CHECK(command==7 && last_submitted.operation==AP_AUXILIARY_FILE && last_submitted.expected_object==19);
    FILE_DRIVER_CHECK(last_submitted.generation_before==72 && last_submitted.expected_option==3 && last_submitted.expected_level==5 && last_submitted.generation_after==2);
    unsigned submitted=submission_count;
    FILE_DRIVER_CHECK(ap_prepare_auxiliary_file(&s,9,0,2,5,&command)==-1 && errno==EINVAL);
    FILE_DRIVER_CHECK(ap_prepare_auxiliary_file(&s,9,19,2,-1,&command)==-1 && errno==EINVAL);
    FILE_DRIVER_CHECK(submission_count==submitted);
    FILE_DRIVER_CHECK(ap_read_original_selection(&s,9,7,&selected)==0 && selected.file==53);
    FILE_DRIVER_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==-1 && errno==ENODATA);
    FILE_DRIVER_CHECK(!s.pending[7].original_collected && collect_calls==0);
    command_result.phase=AP_COMMAND_DONE;command_result.returned=2048;
    original_call.original.complete=1;original_call.original.returned=2048;
    FILE_DRIVER_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==0 && final.returned==2048);
    FILE_DRIVER_CHECK(fd_ack_original(&s,&s.pending[7])==0 && original_absent);
    auxiliary_file_reset(&s);original_call.original.selection.file=0;original_call.original.selection.fdput_flags=0;
    FILE_DRIVER_CHECK(ap_read_original_selection(&s,9,7,&selected)==0 && !selected.file);
    command_result.phase=AP_COMMAND_DONE;original_call.original.complete=1;
    FILE_DRIVER_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==-1 && errno==ENODATA);
    command_result.returned=-EBADF;original_call.original.returned=-EBADF;
    FILE_DRIVER_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==0);
    FILE_DRIVER_CHECK(fd_ack_original(&s,&s.pending[7])==0 && original_absent);
    for(unsigned bad=0;bad<9;bad++) {
        auxiliary_file_reset(&s);
        if(bad==0)original_call.original.selection.ready=0;if(bad==1)original_call.original.selection.user_address=71;
        if(bad==2)original_call.original.selection.address_length=4;if(bad==3)original_call.original.selection.owner_mm++;
        if(bad==4)original_call.original.selection.provider++;if(bad==5)original_task.command++;
        if(bad==6)command_result.identity.provider++;if(bad==7)original_torn_component=1;if(bad==8)original_torn_component=2;
        FILE_DRIVER_CHECK(ap_read_original_selection(&s,9,7,&selected)==-1);
        FILE_DRIVER_CHECK(!s.pending[7].original_selected && !s.pending[7].original_collected && collect_calls==0);
    }
    auxiliary_file_reset(&s);terminal_ready=true;struct ap_original_terminal terminal;
    FILE_DRIVER_CHECK(ap_retire_dead_original(&s,9,7,&terminal)==0);
    FILE_DRIVER_CHECK(terminal.fd_call_present && terminal.original.selection.file==53 && !terminal.original.complete);
    FILE_DRIVER_CHECK(terminal.task_absent && original_absent);
    assert(checks==36);printf("auxiliary File23 common driver: %u checks\n",checks);
#undef FILE_DRIVER_CHECK
}

static void original_socket_reset(struct ap_session *s) {
    original_reset(s);
    s->pending[7].submitted.operation=AP_ORIGINAL_SOCKET_CALL;
    s->pending[7].submitted.expected_level=2;
    s->pending[7].submitted.generation_before=1;
    s->pending[7].submitted.expected_option=6;
    original_task=s->pending[7].submitted;command_result.operation=AP_ORIGINAL_SOCKET_CALL;
    original_call.operation=AP_ORIGINAL_SOCKET_CALL;original_call.new_file=0x5000;
    original_call.original.selection.requested_fd=2;
    original_call.original.selection.user_address=1;
    original_call.original.selection.address_length=6;
    original_call.original.selection.fdput_flags=0;
    original_call.install_begin=41;
    original_call.original.installation=(struct ap_original_socket_installation){.begin=41,.end=42,.fd=8};
}
static void original_socket_driver_controls(void) {
    unsigned checks=0;
#define SOCKET_DRIVER_CHECK(x) do {assert(x);checks++;}while(0)
    struct ap_session s;u64 command=0;struct ap_original_selection selected;
    struct ap_command_result result;struct ap_original_result final;
    original_socket_reset(&s);
    SOCKET_DRIVER_CHECK(ap_prepare_original_socket(&s,9,19,2,2,1,6,&command)==0);
    SOCKET_DRIVER_CHECK(command==7 && last_submitted.operation==AP_ORIGINAL_SOCKET_CALL);
    SOCKET_DRIVER_CHECK(last_submitted.expected_object==19 && last_submitted.generation_after==2);
    SOCKET_DRIVER_CHECK(last_submitted.expected_level==2 && last_submitted.generation_before==1 && last_submitted.expected_option==6 && !last_submitted.original_count);
    unsigned submissions=submission_count;
    SOCKET_DRIVER_CHECK(ap_prepare_original_socket(&s,9,0,2,2,1,6,&command)==-1 && errno==EINVAL);
    SOCKET_DRIVER_CHECK(submission_count==submissions);
    SOCKET_DRIVER_CHECK(ap_read_original_selection(&s,9,7,&selected)==0 && selected.file==53);
    SOCKET_DRIVER_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==-1 && errno==ENODATA);
    SOCKET_DRIVER_CHECK(!s.pending[7].original_collected && collect_calls==0);
    command_result.phase=AP_COMMAND_DONE;command_result.returned=8;
    original_call.original.complete=1;original_call.original.returned=8;
    SOCKET_DRIVER_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==0 && final.returned==8 && final.installation.begin==41 && final.installation.end==42);
    SOCKET_DRIVER_CHECK(fd_ack_original(&s,&s.pending[7])==0 && original_absent);
    original_socket_reset(&s);original_call.original.selection.file=0;
    memset(original_call.original.address,0,sizeof(original_call.original.address));
    original_call.install_begin=original_call.new_file=0;
    SOCKET_DRIVER_CHECK(ap_read_original_selection(&s,9,7,&selected)==0 && !selected.file);
    command_result.phase=AP_COMMAND_DONE;command_result.returned=-24;
    original_call.original.complete=1;original_call.original.returned=-24;
    SOCKET_DRIVER_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==0 && final.returned==-24);
    SOCKET_DRIVER_CHECK(fd_ack_original(&s,&s.pending[7])==0 && original_absent);
    for(unsigned bad=0;bad<5;bad++) {
        original_socket_reset(&s);command_result.phase=AP_COMMAND_DONE;command_result.returned=8;
        original_call.original.complete=1;original_call.original.returned=8;
        if(bad==0)original_call.original.installation.end=41;
        if(bad==1)original_call.original.installation.fd=9;
        if(bad==2)original_call.original.selection.ready=0;
        if(bad==3)original_call.original.address[127]=1;
        if(bad==4)original_torn_component=2;
        SOCKET_DRIVER_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==-1);
        SOCKET_DRIVER_CHECK(!s.pending[7].original_collected && collect_calls==0);
    }
    assert(checks==24);printf("original Socket common driver: %u checks\n",checks);
#undef SOCKET_DRIVER_CHECK
}

static void original_epoll_driver_controls(void) {
    for(unsigned legacy=0;legacy<2;legacy++) {
        int nr=legacy?AP_EPOLL_CREATE_SYSCALL:AP_EPOLL_CREATE1_SYSCALL;
        int argument=legacy?1:AP_EPOLL_CLOEXEC;
        struct ap_session s;original_socket_reset(&s);u64 command=0;
        assert(ap_prepare_original_epoll(&s,9,19,2,nr,argument,&command)==0);
        assert(command==7 && last_submitted.operation==AP_ORIGINAL_EPOLL_CALL &&
            last_submitted.expected_object==19 && last_submitted.generation_after==2 &&
            last_submitted.generation_before==(u64)nr && last_submitted.expected_level==argument &&
            !last_submitted.expected_option && !last_submitted.original_count);
        unsigned before=submission_count;
        assert(ap_prepare_original_epoll(&s,9,19,2,41,argument,&command)==-1 && errno==EINVAL);
        assert(submission_count==before);
        struct ap_task_command *c=&s.pending[7].submitted;
        c->operation=AP_ORIGINAL_EPOLL_CALL;c->generation_before=nr;c->expected_level=argument;c->expected_option=0;
        original_task=*c;command_result.operation=AP_ORIGINAL_EPOLL_CALL;
        original_call.operation=AP_ORIGINAL_EPOLL_CALL;
        original_call.original.selection.requested_fd=argument;
        original_call.original.selection.user_address=nr;
        original_call.original.selection.address_length=0;
        original_call.original.epoll.status_flags=2;original_call.original.epoll.descriptor_flags=1;
        original_call.original.epoll.profiled=1;
        struct ap_command_result result;struct ap_original_result final;
        assert(ap_collect_original_connect(&s,9,7,&result,&final)==-1 && errno==ENODATA);
        assert(!s.pending[7].original_collected && collect_calls==0);
        command_result.phase=AP_COMMAND_DONE;command_result.returned=8;
        original_call.original.complete=1;original_call.original.returned=8;
        /* DONE is not a substitute for retaining the immutable selection.
         * Use the real shared API and preserve the refused caller output. */
        memset(&final,0x5a,sizeof(final));struct ap_original_result untouched=final;
        assert(ap_collect_original_connect(&s,9,7,&result,&final)==-1 && errno==ENODATA);
        assert(!s.pending[7].original_selected && !s.pending[7].original_collected &&
            !collect_calls && !memcmp(&final,&untouched,sizeof(final)));
        struct ap_original_selection selected;
        assert(ap_read_original_selection(&s,9,7,&selected)==0 && selected.file==53 &&
            selected.requested_fd==argument && selected.user_address==(u64)nr &&
            !selected.address_length && !selected.fdput_flags && selected.ready==1);
        assert(s.pending[7].original_selected && !s.pending[7].original_collected && !collect_calls);
        assert(ap_collect_original_connect(&s,9,7,&result,&final)==0);
        assert(final.installation.begin==41 && final.installation.end==42 && final.installation.fd==8 &&
            final.epoll.status_flags==2 && final.epoll.descriptor_flags==1 && collect_calls==1);
        assert(fd_ack_original(&s,&s.pending[7])==0 && original_absent);
    }
    puts("original epoll common driver: original command collection and exact ACK custody");
}

static void ctl_reset(struct ap_session *s) {
    original_reset(s);
    struct ap_task_command *c=&s->pending[7].submitted;
    c->operation=AP_ORIGINAL_EPOLL_CTL;c->expected_option=1;c->original_count=6;original_task=*c;
    command_result.operation=AP_ORIGINAL_EPOLL_CTL;command_result.original_count=6;
    original_call.operation=AP_ORIGINAL_EPOLL_CTL;
    original_call.original.selection.address_length=1;original_call.original.selection.original_count=6;
    original_call.original.epoll_ctl=(struct ap_original_epoll_ctl){.entered=1,.ctl_entered=1,
        .primary_selected=1,.secondary_selected=1,.target_file=59,.target_flags=1,
        .before=2,.primary_cut=3,.target_cut=4,.image_wakeup_policy=1};
    original_call.original.epoll_ctl.event[0]=1;original_call.original.epoll_ctl.event[7]=0xad;
}
static void ctl_return(void) {
    command_result.phase=AP_COMMAND_DONE;command_result.returned=0;
    original_call.original.returned=0;original_call.original.complete=1;
    original_call.original.epoll_ctl.ctl_returned=1;
}
static void original_ctl_driver_controls(void) {
    struct ap_session s;struct ap_original_selection primary;
    struct ap_original_result selected,completed;struct ap_command_result result;
    ctl_reset(&s);u64 command=0;
    assert(ap_prepare_original_epoll_ctl(&s,9,19,2,-1,-99,-1,~0ULL,&command)==0);
    assert(command==7 && last_submitted.operation==20 && last_submitted.expected_level==-1 &&
        last_submitted.expected_option==-99 && last_submitted.original_count==0xffffffffULL &&
        last_submitted.generation_before==~0ULL); // Kernel owns input/error ordering.
    assert(ap_read_original_selection(&s,9,7,&primary)==-1 && errno==EINVAL);
    assert(!s.pending[7].original_selected);
    assert(ap_read_original_epoll_ctl_selection(&s,9,7,&selected)==0);
    assert(selected.epoll_ctl.target_file==59 && selected.epoll_ctl.event[7]==0xad);
    assert(s.pending[7].original_selected && !s.pending[7].original_collected && !collect_calls);
    ctl_return();assert(ap_collect_original_connect(&s,9,7,&result,&completed)==0);
    assert(!memcmp(selected.address,completed.address,96) && collect_calls==1);
    assert(fd_ack_original(&s,&s.pending[7])==0 && original_absent);
    for(unsigned field=0;field<4;field++) {
        ctl_reset(&s);assert(ap_read_original_epoll_ctl_selection(&s,9,7,&selected)==0);ctl_return();
        if(field==0)original_call.original.epoll_ctl.target_file++;
        if(field==1)original_call.original.epoll_ctl.event[7]++;
        if(field==2)original_call.original.epoll_ctl.target_cut++;
        if(field==3)original_call.original.epoll_ctl.before++;
        assert(ap_collect_original_connect(&s,9,7,&result,&completed)==-1);
        assert(!s.pending[7].original_collected && !collect_calls && !original_absent);
    }
    for(unsigned torn=1;torn<=4;torn++) {
        ctl_reset(&s);original_torn_component=torn;
        assert(ap_read_original_epoll_ctl_selection(&s,9,7,&selected)==-1);
        assert(!s.pending[7].original_selected && !collect_calls);
    }
    ctl_reset(&s);original_call.original.epoll_ctl.secondary_selected=0;
    assert(ap_read_original_epoll_ctl_selection(&s,9,7,&selected)==-1 && !s.pending[7].original_selected);
    puts("original ctl common driver: immutable pair/copy retention, native completion and same-command ACK");
}

int main(void) {
    original_ctl_driver_controls();
    original_epoll_driver_controls();
    auxiliary_file_driver_controls();
    original_socket_driver_controls();
    original_file_driver_controls();
    journal_observer_controls();
    original_close_controls();
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
    unsigned observer_checks=0;
#define OBSERVER_CHECK(v) do { assert(v);observer_checks++; } while(0)
    const int failures[]={ENODATA,EIO,ESTALE};
    for(unsigned i=0;i<3;i++) {
        reset(&s);observer_error=failures[i];
        struct ap_fd_accept exact=map_receipt;
        OBSERVER_CHECK(ap_collect_accept(&s,9,7,&result,&receipt)==-1);
        OBSERVER_CHECK(errno==failures[i] && collect_calls==0);
        OBSERVER_CHECK(!s.pending[7].fd_collected && s.pending[7].submitted.command==7);
        OBSERVER_CHECK(!memcmp(&map_receipt,&exact,sizeof(exact)) && receipt.command==7);
    }
    assert(observer_checks==12);
    printf("fdget observer refusal custody: %u checks\n",observer_checks);
    unsigned original_checks=0;
#define ORIGINAL_CHECK(v) do { assert(v);original_checks++; } while(0)
    struct ap_original_selection selected;struct ap_original_result final;
    original_reset(&s);
    ORIGINAL_CHECK(ap_read_original_selection(&s,9,7,&selected)==0);
    ORIGINAL_CHECK(selected.ready==1 && selected.file==53 && selected.fdput_flags==1);
    ORIGINAL_CHECK(s.pending[7].original_selected && !s.pending[7].original_collected);
    ORIGINAL_CHECK(collect_calls==0 && s.pending[7].state==AP_SLOT_ACTIVE && !original_absent);
    ORIGINAL_CHECK(!original_call.original.complete && !original_call.original.security_entered);
    original_return();
    ORIGINAL_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==0);
    ORIGINAL_CHECK(collect_calls==1 && final.returned==-111 && final.address[0]==2);
    ORIGINAL_CHECK(s.pending[7].original_collected && !original_absent);
    original_delete_error=EIO;
    ORIGINAL_CHECK(fd_ack_original(&s,&s.pending[7])==-1 && errno==EIO);
    ORIGINAL_CHECK(!original_absent && s.pending[7].original_collected);
    original_delete_error=0;
    ORIGINAL_CHECK(fd_ack_original(&s,&s.pending[7])==0 && original_absent);
    original_reset(&s);original_return();
    ORIGINAL_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==-1 && errno==ENODATA);
    ORIGINAL_CHECK(!s.pending[7].original_collected && !original_absent && collect_calls==0);
    original_reset(&s);original_call.original.selection.ready=0;
    ORIGINAL_CHECK(ap_read_original_selection(&s,9,7,&selected)==-1 && errno==ENODATA);
    ORIGINAL_CHECK(!s.pending[7].original_selected && !original_absent && collect_calls==0);
    original_reset(&s);original_torn=true;
    ORIGINAL_CHECK(ap_read_original_selection(&s,9,7,&selected)==-1 && errno==ENODATA);
    ORIGINAL_CHECK(!s.pending[7].original_selected && !original_absent);
    for(unsigned i=0;i<3;i++) {
        original_reset(&s);observer_error=failures[i];
        ORIGINAL_CHECK(ap_read_original_selection(&s,9,7,&selected)==-1 && errno==failures[i]);
        ORIGINAL_CHECK(!s.pending[7].original_selected && collect_calls==0 && !original_absent);
    }
    original_reset(&s);
    ORIGINAL_CHECK(ap_read_original_selection(&s,9,7,&selected)==0);
    struct ap_original_selection retained=s.pending[7].original_selection;
    original_call.original.selection.file++;
    ORIGINAL_CHECK(ap_read_original_selection(&s,9,7,&selected)==-1 && errno==ESTALE);
    ORIGINAL_CHECK(!memcmp(&retained,&s.pending[7].original_selection,sizeof(retained)));
    ORIGINAL_CHECK(!original_absent && collect_calls==0);
    original_reset(&s);original_call.original.selection.file=0;
    original_call.original.selection.fdput_flags=0;original_call.selected_file=0;
    original_call.selection.word=0;
    ORIGINAL_CHECK(ap_read_original_selection(&s,9,7,&selected)==0);
    command_result.phase=AP_COMMAND_DONE;command_result.returned=-9;
    original_call.original.complete=1;original_call.original.returned=-9;
    ORIGINAL_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==0);
    ORIGINAL_CHECK(!final.selection.file && !final.security_entered && final.returned==-9);
    ORIGINAL_CHECK(fd_ack_original(&s,&s.pending[7])==0 && original_absent);
    /* __fdget's files->count==1 path returns a borrowed non-null file. */
    original_reset(&s);original_call.original.selection.fdput_flags=0;
    original_call.selection.word=0x5000;
    ORIGINAL_CHECK(ap_read_original_selection(&s,9,7,&selected)==0 && selected.file==53 && !selected.fdput_flags);
    original_return();
    ORIGINAL_CHECK(ap_collect_original_connect(&s,9,7,&result,&final)==0);
    ORIGINAL_CHECK(fd_ack_original(&s,&s.pending[7])==0 && original_absent);
    /* A raw ENOTSOCK with no security session is retained as UNKNOWN; only
     * actual retained non-socket pin custody can classify that native path. */
    original_reset(&s);original_return();
    original_call.original.security_entered=original_call.original.security_returned=0;
    memset(original_call.original.address,0,sizeof(original_call.original.address));
    original_call.original.returned=command_result.returned=-ENOTSOCK;
    ORIGINAL_CHECK(ap_original_result_matches(&s.pending[7].submitted,&command_result,&original_call.original));
    ORIGINAL_CHECK(ap_original_path(&original_call.original)==AP_ORIGINAL_UNKNOWN);
    original_call.original.copy_remaining=2;original_call.original.returned=command_result.returned=-EFAULT;
    ORIGINAL_CHECK(ap_original_result_matches(&s.pending[7].submitted,&command_result,&original_call.original));
    ORIGINAL_CHECK(ap_original_path(&original_call.original)==AP_ORIGINAL_COPY_FAULT);
    original_call.original.security_entered=1;
    ORIGINAL_CHECK(!ap_original_result_matches(&s.pending[7].submitted,&command_result,&original_call.original));
    /* Exercise the actual C disarm. This fixture supplies the separate trusted
     * known-uninvoked authority; a READY command is not that authority itself. */
    unsigned cancel_checks=0;
#define CANCEL_CHECK(v) do { assert(v);cancel_checks++; } while(0)
    original_reset(&s);
    command_result=(struct ap_command_result){.command=7,.operation=AP_ORIGINAL_CONNECT,.phase=AP_COMMAND_READY};
    CANCEL_CHECK(ap_cancel_uninvoked_original(&s,9,7)==0);
    CANCEL_CHECK(s.pending[7].state==AP_SLOT_FREE && !s.pending[7].submitted.command);
    CANCEL_CHECK(!command_result.command && original_task.provider==3 && !original_task.command && collect_calls==0);
    CANCEL_CHECK(ap_cancel_uninvoked_original(&s,9,7)==-1 && errno==EINVAL);
    original_reset(&s);
    CANCEL_CHECK(ap_cancel_uninvoked_original(&s,9,7)==-1 && errno==ESTALE);
    CANCEL_CHECK(s.pending[7].state==AP_SLOT_ACTIVE && original_task.command==7 && command_result.phase==AP_COMMAND_RUNNING);
    original_reset(&s);
    command_result=(struct ap_command_result){.command=7,.operation=AP_ORIGINAL_CONNECT,.phase=AP_COMMAND_READY};
    original_task.generation_after++;
    CANCEL_CHECK(ap_cancel_uninvoked_original(&s,9,7)==-1 && errno==ESTALE);
    CANCEL_CHECK(s.pending[7].state==AP_SLOT_ACTIVE && command_result.command==7);
    original_reset(&s);
    command_result=(struct ap_command_result){.command=7,.operation=AP_ORIGINAL_CONNECT,.phase=AP_COMMAND_READY};
    s.pending[7].original_selected=true;
    CANCEL_CHECK(ap_cancel_uninvoked_original(&s,9,7)==-1 && errno==EINVAL);
    CANCEL_CHECK(s.pending[7].state==AP_SLOT_ACTIVE && original_task.command==7);
    original_reset(&s);
    command_result=(struct ap_command_result){.command=7,.operation=AP_ORIGINAL_CONNECT,.phase=AP_COMMAND_READY};
    update_error=1;
    CANCEL_CHECK(ap_cancel_uninvoked_original(&s,9,7)==-1 && errno==EBUSY);
    CANCEL_CHECK(s.pending[7].state==AP_SLOT_QUARANTINED && command_result.command==7 && collect_calls==0);
    assert(cancel_checks==12);
    printf("known-uninvoked original disarm: %u checks\n",cancel_checks);
    printf("original connect retained selection/return: %u checks\n",original_checks);
    unsigned birth_checks=0;struct ap_native_birth birth;
#define BIRTH_CHECK(v) do {assert(v);birth_checks++;} while(0)
    birth_reset(&s);
    BIRTH_CHECK(ap_admit_native_birth_child(&s,12,7,&birth)==0);
    BIRTH_CHECK(child_marker_absent && s.pending[7].birth_child_admitted && !s.pending[7].birth_child_terminal);
    BIRTH_CHECK(ap_admit_native_birth_child(&s,12,7,&birth)==-1 && errno==ESTALE);
    BIRTH_CHECK(ap_admit_native_birth_terminal(&s,7,&birth)==-1 && errno==ESTALE);
    command_result.phase=AP_COMMAND_DONE;
    BIRTH_CHECK(ap_collect_native_birth(&s,9,7,&result,&birth)==0);
    BIRTH_CHECK(fd_ack_birth(&s,&s.pending[7])==0 && original_absent);
    birth_reset(&s);child_marker.command=8;
    BIRTH_CHECK(ap_admit_native_birth_child(&s,12,7,&birth)==-1 && errno==ESTALE);
    BIRTH_CHECK(!child_marker_absent && !s.pending[7].birth_child_admitted);
    birth_reset(&s);child_marker_absent=true;
    BIRTH_CHECK(ap_admit_native_birth_child(&s,12,7,&birth)==-1 && errno==ENOENT);
    // Terminal consumption is separately authorized by the real backend event;
    // absence did not make the live operation successful.
    BIRTH_CHECK(ap_admit_native_birth_terminal(&s,7,&birth)==0);
    BIRTH_CHECK(s.pending[7].birth_child_terminal && !s.pending[7].birth_child_admitted);
    birth_reset(&s);original_call.birth.ready=0;
    BIRTH_CHECK(ap_read_native_birth(&s,7,&birth)==-1 && errno==ENODATA);
    BIRTH_CHECK(!s.pending[7].birth_observed);
    birth_reset(&s);command_result.phase=AP_COMMAND_DONE;command_result.returned=-EAGAIN;
    original_call.birth.ready=0; // Provisional chosen-parent witness remains raw evidence.
    BIRTH_CHECK(ap_collect_native_birth(&s,9,7,&result,&birth)==0 && result.returned==-EAGAIN);
    BIRTH_CHECK(!s.pending[7].birth_child_admitted && !s.pending[7].birth_child_terminal);
    BIRTH_CHECK(fd_ack_birth(&s,&s.pending[7])==0 && original_absent);
    birth_reset(&s);command_result=(struct ap_command_result){.command=7,.operation=AP_NATIVE_BIRTH,.phase=AP_COMMAND_READY};
    BIRTH_CHECK(ap_cancel_uninvoked_birth(&s,9,7)==0);
    BIRTH_CHECK(s.pending[7].state==AP_SLOT_FREE && !command_result.command && !original_task.command);
    birth_reset(&s);
    BIRTH_CHECK(ap_cancel_uninvoked_birth(&s,9,7)==-1 && errno==ESTALE);
    BIRTH_CHECK(s.pending[7].state==AP_SLOT_ACTIVE && original_task.command==7);
    assert(birth_checks==20);
    printf("native birth retained dispatch: %u checks\n",birth_checks);
    unsigned birth_shape_checks=0;u64 shape_command=0;
#define SHAPE_CHECK(v) do {assert(v);birth_shape_checks++;} while(0)
    birth_reset(&s);submission_count=0;
    SHAPE_CHECK(ap_prepare_native_birth(&s,9,17,23,47,435,&shape_command)==0);
    SHAPE_CHECK(submission_count==1 && shape_command==7 && last_submitted.expected_level==435 && last_submitted.expected_option==0);
    birth_reset(&s);submission_count=0;
    SHAPE_CHECK(ap_prepare_native_birth(&s,9,17,23,47,60,&shape_command)==-1);
    SHAPE_CHECK(submission_count==0);
    assert(birth_shape_checks==4);
    printf("native birth syscall-shape dispatch: %u checks\n",birth_shape_checks);
    unsigned terminal_checks=0;
#define TERMINAL_CHECK(v) do { assert(v);terminal_checks++; } while(0)
    struct ap_original_terminal terminal;
    const u64 terminal_phases[]={AP_COMMAND_READY,AP_COMMAND_RUNNING,AP_COMMAND_DONE};
    for(unsigned n=0;n<3;n++) {
        original_reset(&s);terminal_ready=true;
        if(n==0) {
            command_result=(struct ap_command_result){.command=7,.operation=AP_ORIGINAL_CONNECT,.phase=AP_COMMAND_READY};
            original_absent=true;
        } else if(n==2)original_return();
        struct ap_command_result before=command_result;
        struct ap_original_result raw=original_call.original;
        TERMINAL_CHECK(ap_retire_dead_original(&s,9,7,&terminal)==0);
        TERMINAL_CHECK(terminal.command.phase==terminal_phases[n] && !memcmp(&terminal.command,&before,sizeof(before)));
        TERMINAL_CHECK(terminal.task_absent==1 && terminal.fd_call_present==(n!=0));
        TERMINAL_CHECK(!n || !memcmp(&terminal.original,&raw,sizeof(raw)));
        TERMINAL_CHECK(s.pending[7].state==AP_SLOT_FREE && !command_result.command && task_absent && original_absent && !collect_calls);
    }
    original_reset(&s);
    TERMINAL_CHECK(ap_retire_dead_original(&s,9,7,&terminal)==-1 && errno==EAGAIN);
    TERMINAL_CHECK(s.pending[7].state==AP_SLOT_ACTIVE && !task_absent && !original_absent);
    terminal_ready=true;
    TERMINAL_CHECK(ap_retire_dead_original(&s,10,7,&terminal)==-1 && errno==EAGAIN);
    TERMINAL_CHECK(s.pending[7].state==AP_SLOT_ACTIVE && !task_absent && !original_absent);
    terminal_poll_error=EINTR;
    TERMINAL_CHECK(ap_retire_dead_original(&s,9,7,&terminal)==-1 && errno==EINTR);
    TERMINAL_CHECK(s.pending[7].state==AP_SLOT_ACTIVE && !task_absent && !original_absent);
    original_reset(&s);terminal_ready=true;original_call.original.selection.task_start++;
    TERMINAL_CHECK(ap_retire_dead_original(&s,9,7,&terminal)==-1 && errno==ESTALE);
    TERMINAL_CHECK(s.pending[7].state==AP_SLOT_ACTIVE && !task_absent && !original_absent);
    original_reset(&s);terminal_ready=true;original_delete_error=EIO;
    TERMINAL_CHECK(ap_retire_dead_original(&s,9,7,&terminal)==-1 && errno==EIO);
    TERMINAL_CHECK(s.pending[7].state==AP_SLOT_QUARANTINED && command_result.command==7 && !original_absent);
    original_reset(&s);terminal_ready=true;task_absent=true;
    TERMINAL_CHECK(ap_retire_dead_original(&s,9,7,&terminal)==0);
    TERMINAL_CHECK(terminal.task_absent==1 && terminal.fd_call_present==1 && original_absent && !command_result.command);
    original_reset(&s);terminal_ready=true;original_absent=true;
    TERMINAL_CHECK(ap_retire_dead_original(&s,9,7,&terminal)==0);
    TERMINAL_CHECK(terminal.fd_call_present==0 && terminal.command.phase==AP_COMMAND_RUNNING && !terminal.original.complete);
    assert(terminal_checks==29);
    printf("dead original exact physical retirement: %u checks\n",terminal_checks);
    unsigned birth_terminal_checks=0;struct ap_native_birth_terminal bt;
#define BIRTH_TERMINAL(v) do {assert(v);birth_terminal_checks++;} while(0)
    for(unsigned phase=0;phase<2;phase++)for(unsigned dead_child=0;dead_child<2;dead_child++) {
        birth_reset(&s);terminal_ready=true;
        command_result.phase=phase?AP_COMMAND_DONE:AP_COMMAND_RUNNING;
        if(!phase) {command_result.identity.provider=0;command_result.returned=0;}
        BIRTH_TERMINAL((dead_child?ap_admit_native_birth_terminal(&s,7,&birth):
            ap_admit_native_birth_child(&s,12,7,&birth))==0);
        struct ap_command_result raw=command_result;struct ap_native_birth old=original_call.birth;
        BIRTH_TERMINAL(ap_retire_dead_birth(&s,9,7,&bt)==0);
        BIRTH_TERMINAL(!memcmp(&raw,&bt.command,sizeof(raw)));
        BIRTH_TERMINAL(!memcmp(&old,&bt.birth,sizeof(old)) && bt.task_absent==1 && bt.fd_call_present==1 && bt.call==17);
        BIRTH_TERMINAL(collect_calls==0 && (phase || bt.command.returned==0));
        BIRTH_TERMINAL(s.pending[7].state==AP_SLOT_FREE && task_absent && original_absent && !command_result.command);
    }
    birth_reset(&s);terminal_ready=true;
    BIRTH_TERMINAL(ap_retire_dead_birth(&s,9,7,&bt)==-1 && errno==EAGAIN);
    BIRTH_TERMINAL(s.pending[7].state==AP_SLOT_ACTIVE && !task_absent && !original_absent);
    birth_reset(&s);assert(ap_admit_native_birth_child(&s,12,7,&birth)==0);
    BIRTH_TERMINAL(ap_retire_dead_birth(&s,9,7,&bt)==-1 && errno==EAGAIN);
    BIRTH_TERMINAL(s.pending[7].state==AP_SLOT_ACTIVE && !task_absent && !original_absent);
    terminal_ready=true;
    BIRTH_TERMINAL(ap_retire_dead_birth(&s,10,7,&bt)==-1 && errno==EAGAIN);
    BIRTH_TERMINAL(s.pending[7].state==AP_SLOT_ACTIVE && !task_absent && !original_absent);
    terminal_poll_error=EINTR;
    BIRTH_TERMINAL(ap_retire_dead_birth(&s,9,7,&bt)==-1 && errno==EINTR);
    BIRTH_TERMINAL(s.pending[7].state==AP_SLOT_ACTIVE && !task_absent && !original_absent);
    birth_reset(&s);terminal_ready=true;assert(ap_admit_native_birth_child(&s,12,7,&birth)==0);
    original_call.birth.creator_start++;
    BIRTH_TERMINAL(ap_retire_dead_birth(&s,9,7,&bt)==-1);
    BIRTH_TERMINAL(s.pending[7].state==AP_SLOT_ACTIVE && !task_absent && !original_absent);
    birth_reset(&s);terminal_ready=true;assert(ap_admit_native_birth_child(&s,12,7,&birth)==0);
    original_call.birth.child_start++;
    BIRTH_TERMINAL(ap_retire_dead_birth(&s,9,7,&bt)==-1 && errno==ESTALE);
    BIRTH_TERMINAL(s.pending[7].state==AP_SLOT_ACTIVE && !task_absent && !original_absent);
    birth_reset(&s);terminal_ready=true;assert(ap_admit_native_birth_child(&s,12,7,&birth)==0);original_absent=true;
    BIRTH_TERMINAL(ap_retire_dead_birth(&s,9,7,&bt)==-1 && errno==ENOENT);
    BIRTH_TERMINAL(s.pending[7].state==AP_SLOT_ACTIVE && !task_absent && command_result.command==7);
    birth_reset(&s);terminal_ready=true;assert(ap_admit_native_birth_child(&s,12,7,&birth)==0);original_delete_error=EIO;
    BIRTH_TERMINAL(ap_retire_dead_birth(&s,9,7,&bt)==-1 && errno==EIO);
    BIRTH_TERMINAL(s.pending[7].state==AP_SLOT_QUARANTINED && task_absent && !original_absent && command_result.command==7);
    birth_reset(&s);terminal_ready=true;assert(ap_admit_native_birth_child(&s,12,7,&birth)==0);
    command_result.phase=AP_COMMAND_DONE;command_result.returned=0;
    BIRTH_TERMINAL(ap_retire_dead_birth(&s,9,7,&bt)==-1 && errno==EPROTO);
    BIRTH_TERMINAL(s.pending[7].state==AP_SLOT_ACTIVE && !task_absent && !original_absent);
    assert(birth_terminal_checks==42);
    printf("native birth dead-creator retirement: %u checks\n",birth_terminal_checks);
    return 0;
}
