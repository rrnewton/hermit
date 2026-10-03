/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define main retained_terminal_controls
#define __wrap_syscall retained_syscall
#define __wrap_write retained_write
#include "keeper-terminal-tests.c"
#undef main
#undef __wrap_syscall
#undef __wrap_write
static u64 clock_ns;
static int queries,clock_failure,clock_calls,exported;
static struct ug_inventory exported_inventory;
static bool probe_control, probe_present;
static struct ug_probe probe_value;
static struct ug_task probe_task;
static int probe_update_failure, probe_delete_failure, probe_read_failure;
static unsigned probe_updates, probe_deletes, probe_reads, probe_read_fail_at;

struct bpf_map *bpf_object__next_map(const struct bpf_object *object,const struct bpf_map *map) {
    assert(object==(void *)123);uintptr_t n=(uintptr_t)map;
    return n==UG_MAPS?NULL:(void *)(n+1);
}
int bpf_map__fd(const struct bpf_map *map) {return 199+(int)(uintptr_t)map;}
struct bpf_program *bpf_object__next_program(const struct bpf_object *object,struct bpf_program *program) {
    assert(object==(void *)123);uintptr_t n=(uintptr_t)program;
    return n==UG_LINKS?NULL:(void *)(n+1);
}
int bpf_program__fd(const struct bpf_program *program) {return 499+(int)(uintptr_t)program;}
int bpf_link__fd(const struct bpf_link *link) {return 199+UG_MAPS+(int)(uintptr_t)link;}
long __wrap_syscall(long nr,...) {
    va_list ap;va_start(ap,nr);
    if(nr==SYS_memfd_create) {va_end(ap);exported++;return 800;}
    assert(nr==SYS_bpf);int cmd=va_arg(ap,int);union bpf_attr *a=va_arg(ap,union bpf_attr *);
    size_t size=va_arg(ap,size_t);va_end(ap);
    if(probe_control && cmd==BPF_MAP_LOOKUP_ELEM && a->map_fd==104) {
        assert(*(int *)(uintptr_t)a->key==32 && !a->flags);
        memcpy((void *)(uintptr_t)a->value,&probe_task,sizeof(probe_task));return 0;
    }
    if(probe_control && a->map_fd==109 &&
       (cmd==BPF_MAP_LOOKUP_ELEM || cmd==BPF_MAP_UPDATE_ELEM || cmd==BPF_MAP_DELETE_ELEM)) {
        assert(*(int *)(uintptr_t)a->key==32);
        if(cmd==BPF_MAP_LOOKUP_ELEM) {
            assert(!a->flags);
            probe_reads++;
            if(probe_read_failure || probe_reads==probe_read_fail_at) {errno=EIO;return -1;}
            if(!probe_present) {errno=ENOENT;return -1;}
            memcpy((void *)(uintptr_t)a->value,&probe_value,sizeof(probe_value));return 0;
        }
        if(cmd==BPF_MAP_UPDATE_ELEM) {
            probe_updates++;
            if(probe_update_failure==1) {errno=EIO;return -1;}
            assert(a->flags==BPF_NOEXIST || a->flags==BPF_EXIST);
            if(a->flags==BPF_NOEXIST && probe_present) {errno=EEXIST;return -1;}
            if(a->flags==BPF_EXIST && !probe_present) {errno=ENOENT;return -1;}
            memcpy(&probe_value,(void *)(uintptr_t)a->value,sizeof(probe_value));probe_present=true;
            if(probe_update_failure==2) {errno=EIO;return -1;}
            return 0;
        }
        probe_deletes++;
        if(probe_delete_failure==1) {errno=EIO;return -1;}
        if(!probe_present) {errno=ENOENT;return -1;}probe_present=false;
        if(probe_delete_failure==2) {errno=EIO;return -1;}
        if(probe_delete_failure==3)probe_present=true;
        return 0;
    }
    if(cmd==BPF_LINK_GET_FD_BY_ID || cmd==BPF_MAP_GET_FD_BY_ID || cmd==BPF_PROG_GET_FD_BY_ID)queries++;
    if(cmd==BPF_OBJ_GET_INFO_BY_FD && a->info.bpf_fd>=500 && a->info.bpf_fd<531) {
        ((struct bpf_prog_info *)(uintptr_t)a->info.info)->id=2000+a->info.bpf_fd-500;return 0;
    }
    return retained_syscall(nr,cmd,a,size);
}
ssize_t __wrap_write(int fd,const void *bytes,size_t size) {
    if(fd==800) {assert(size==sizeof(exported_inventory));memcpy(&exported_inventory,bytes,size);return (ssize_t)size;}
    return retained_write(fd,bytes,size);
}
int __wrap_fcntl(int fd,int cmd,...) {assert(fd==800 && cmd==F_ADD_SEALS);return 0;}
int __wrap_clock_gettime(clockid_t id,struct timespec *out) {
    assert(id==CLOCK_MONOTONIC);clock_calls++;
    if(clock_failure && clock_calls==clock_failure) {errno=EIO;return -1;}
    out->tv_sec=(time_t)(clock_ns/1000000000);out->tv_nsec=(long)(clock_ns%1000000000);return 0;
}
static struct ug_terminal_receipt prepare(void) {
    fresh();queries=clock_failure=clock_calls=exported=0;clock_ns=1000000000;
    struct ug_terminal_receipt proof;int inventory=-1;
    assert(ug_session_prepare_terminal(&subject,10,&proof,&inventory)==0);
    assert(inventory==800 && exported==1 && queries==0 && !object_closed);
    assert(subject.close_prepared && !subject.released && destroys==31 && unlinks==41);
    assert(exported_inventory.count==72 && exported_inventory.maps==10 &&
           exported_inventory.programs==31 && exported_inventory.links==31);
    assert(exported_inventory.proof_sequence==10 && exported_inventory.record_ordinal==proof.record_ordinal);
    return proof;
}
static void committed_policy(void) {
    status_value.first_outcome=UG_EVENT_DENIAL;
    status_value.first_denial=(struct ug_denial){.phase=2,.incarnation=7,.reason=UG_FOREIGN_PEER};
}
static unsigned policy_rows(u64 sequence) {
    unsigned found=0;bool terminal=false;
    for(unsigned i=0;i<writes;i++) {
        const struct recovery_record *r=&journal[i];
        if(r->phase==RECORD_TERMINAL)terminal=true;
        if(r->phase==RECORD_FAILURE && !r->error) {
            assert(!terminal && r->sequence==sequence && !r->kind && !r->id);
            assert(!strcmp(r->pin_name,UG_JOURNAL_POLICY_OBSERVED));found++;
        }
    }
    return found;
}
static unsigned failure_rows(void) {
    unsigned found=0;
    for(unsigned i=0;i<writes;i++)if(journal[i].phase==RECORD_FAILURE && journal[i].error) {
        assert(!journal[i].pin_name[0]);found++;
    }
    return found;
}
static void phase_fresh(void) {
    fresh();queries=clock_failure=clock_calls=exported=0;clock_ns=1000000000;
}
static struct ug_terminal_receipt phase_prepare(void) {
    struct ug_terminal_receipt proof;int inventory=-1;
    assert(ug_session_prepare_terminal(&subject,10,&proof,&inventory)==0);
    assert(inventory==800 && exported_inventory.count==72 && queries==0);
    assert(subject.close_prepared && !object_closed && destroys==31 && unlinks==41);
    return proof;
}
static void phase_close(struct ug_terminal_receipt proof,bool clean_suffix) {
    struct ug_object_close closed;
    assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal,4000000000,&closed)==0);
    assert(object_closed && !dir_present && closed.count==72 && queries==0);
    assert(closed.closed_ns==1000000000 && closed.deadline_ns==2000000000);
    assert(journal[writes-2].phase==RECORD_OBJECT_CLOSED && journal[writes-1].phase==RECORD_QUERY_DEADLINE);
    if(clean_suffix)assert(closed.record_ordinal==proof.record_ordinal+2);
    else assert(closed.record_ordinal>proof.record_ordinal+2); /* Retained failure, never a clean receipt. */
}
static void provenance_controls(void) {
    /* Actual session + monitor implementation; every native operation is a
     * wrapped premise. These journals are NOT successful kernel receipts. */
    unsigned passed=0;struct ug_terminal_receipt proof;
    phase_fresh();committed_policy();assert(ug_session_monitor(&subject,33,9)==0);
    assert(policy_rows(9)==1 && !failure_rows());proof=phase_prepare();
    unsigned before=writes;assert(ug_session_monitor(&subject,33,10)==0 && writes==before);
    assert(policy_rows(9)==1);phase_close(proof,true);passed++;

    phase_fresh();assert(ug_session_monitor(&subject,33,9)==0 && !writes);
    committed_policy(); /* Committed after the clean loop snapshot, before TERMINAL. */
    proof=phase_prepare();assert(policy_rows(10)==1 && !failure_rows());
    before=writes;assert(ug_session_monitor(&subject,33,10)==0 && writes==before);
    assert(policy_rows(10)==1);phase_close(proof,true);passed++;

    phase_fresh();monitor_lookup_error=EIO;
    assert(ug_session_monitor(&subject,33,9)==-1 && errno==EIO);
    assert(failure_rows()==1 && journal[0].error==EIO);
    monitor_lookup_error=0;committed_policy();proof=phase_prepare();
    assert(proof.first_outcome==UG_EVENT_DENIAL && failure_rows()==1 && !policy_rows(10));
    assert(subject.monitor.primary==UG_INTERNAL_FAILURE);phase_close(proof,true);passed++;

    phase_fresh();committed_policy();assert(ug_session_monitor(&subject,33,9)==0);
    monitor_lookup_error=EIO;assert(ug_session_monitor(&subject,33,9)==-1);
    assert(subject.monitor.primary==UG_POLICY_REFUSAL && subject.monitor.secondary_monitor_failures==UG_MONITOR_LOOKUP);
    assert(policy_rows(9)==1 && failure_rows()==1);monitor_lookup_error=0;
    proof=phase_prepare();assert(proof.first_outcome==UG_EVENT_DENIAL);
    phase_close(proof,true);assert(failure_rows()==1);passed++;

    phase_fresh();committed_policy();proof=phase_prepare();
    monitor_lookup_error=EIO;assert(ug_session_monitor(&subject,33,10)==-1);
    assert(policy_rows(10)==1 && failure_rows()==1);monitor_lookup_error=0;
    phase_close(proof,false);passed++;

    phase_fresh();assert(ug_session_note_failure(&subject,9,ECANCELED)==0);
    committed_policy();proof=phase_prepare();assert(failure_rows()==1 && journal[0].error==ECANCELED);
    assert(!journal[0].pin_name[0] && policy_rows(10)==1);phase_close(proof,true);passed++;

    phase_fresh();proof=phase_prepare();committed_policy(); /* Impossible late publication must refuse. */
    assert(ug_session_monitor(&subject,33,10)==-1 && errno==EPROTO && failure_rows()==1 && !policy_rows(10));
    before=writes;assert(ug_session_monitor(&subject,33,10)==0 && writes==before);
    phase_close(proof,false);passed++;
    printf("guard_keeper_provenance_controls=%u passed; native operations substituted\n",passed);
    assert(passed==7);
}
static void probe_fresh(void) {
    phase_fresh();probe_control=true;probe_present=false;
    probe_update_failure=probe_delete_failure=probe_read_failure=0;
    probe_updates=probe_deletes=probe_reads=probe_read_fail_at=0;
    memset(&probe_value,0,sizeof(probe_value));
    probe_task=(struct ug_task){.incarnation=7,.initial_registration=2};
    initial_value=(struct ug_initial_task){7,UG_INITIAL_LIVE};live_pidfd=32;
}
static struct ug_probe_receipt probe_arm(u64 kind) {
    struct ug_probe_receipt receipt;
    assert(ug_session_probe_arm(&subject,2,3,kind,&receipt)==0);
    assert(receipt.probe.incarnation==7 && receipt.probe.sequence==3 &&
           receipt.probe.phase==UG_PROBE_ARMED && !receipt.probe.observations &&
           !receipt.probe.denied && receipt.initial_sequence==2 && receipt.kind==kind &&
           !receipt.raw_result && probe_present && probe_updates==1);
    return receipt;
}
static void probe_submit(void) {
    struct ug_probe_receipt receipt;
    assert(ug_session_probe_submit(&subject,2,3,&receipt)==0);
    assert(receipt.probe.phase==UG_PROBE_SUBMITTED && !receipt.probe.observations && probe_updates==2);
}
static void probe_complete(u64 raw) {
    struct ug_probe_receipt receipt;
    probe_value.observations=1;
    assert(ug_session_probe_complete(&subject,2,3,raw,&receipt)==0);
    assert(receipt.probe.phase==UG_PROBE_COMPLETED && receipt.probe.observations==1 &&
           receipt.raw_result==raw && probe_updates==3);
}
static void probe_sticky(void) {
    struct ug_probe_receipt receipt;unsigned before=probe_updates;
    assert(subject.failed);
    assert(ug_session_probe_arm(&subject,2,20,UG_PROBE_POLL,&receipt)==-1);
    assert(probe_updates==before && !unlinks && !destroys && !object_closed);
}
static void probe_controls(void) {
    /* Actual session protocol; native maps/PIDFDs are substituted premises.
     * These certificates are controls, never loaded-kernel receipts. */
    unsigned passed=0;struct ug_probe_receipt receipt;
    const u64 outcomes[2][4]={{0,1,(u64)(int64_t)-516,0},
                             {0,1,(u64)(int64_t)-514,(u64)(int64_t)-EINTR}};
    for(u64 kind=UG_PROBE_POLL;kind<=UG_PROBE_PPOLL;kind++)
        for(unsigned i=0;i<(kind==UG_PROBE_POLL?3u:4u);i++) {
            probe_fresh();probe_arm(kind);probe_submit();probe_complete(outcomes[kind-1][i]);
            assert(ug_session_probe_retire(&subject,2,3,UG_PROBE_RETIRE_COMPLETED,&receipt)==0);
            assert(!probe_present && !subject.initial[0].probe_phase &&
                   subject.initial[0].probe_sequence==3 && receipt.raw_result==outcomes[kind-1][i] &&
                   receipt.probe.observations==1 && probe_deletes==1);passed++;
            assert(ug_session_probe_arm(&subject,2,4,kind,&receipt)==0);
            assert(receipt.probe.sequence==4 && probe_present);passed++;
        }
    probe_fresh();probe_arm(UG_PROBE_POLL);
    assert(ug_session_probe_retire(&subject,2,3,UG_PROBE_RETIRE_UNENTERED,&receipt)==0);
    assert(!probe_present && !receipt.probe.observations && receipt.probe.phase==UG_PROBE_ARMED);passed++;
    assert(ug_session_probe_arm(&subject,2,3,UG_PROBE_POLL,&receipt)==-1);probe_sticky();passed++;

#define ARM_REFUSES(change) do { probe_fresh();change;     assert(ug_session_probe_arm(&subject,2,3,UG_PROBE_POLL,&receipt)==-1);     probe_sticky();assert(!probe_updates);passed++; } while(0)
    ARM_REFUSES(subject.initial_count=0);
    ARM_REFUSES(subject.initial[0].live=false);
    ARM_REFUSES(subject.initial[0].pidfd=-1);
    ARM_REFUSES(subject.initial[0].sequence=4);
    ARM_REFUSES(subject.initial[1]=subject.initial[0];subject.initial_count=2);
    ARM_REFUSES(subject.admissions_closed=true);
    ARM_REFUSES(subject.failed=true);
    ARM_REFUSES(subject.journal_error=EIO);
    ARM_REFUSES(live_pidfd=0);
    ARM_REFUSES(probe_task.incarnation=8);
    ARM_REFUSES(probe_task.initial_registration=4);
    ARM_REFUSES(probe_task.descendant=1);
    ARM_REFUSES(probe_task.reserved=1);
    ARM_REFUSES(initial_value.incarnation=8);
    ARM_REFUSES(initial_value.phase=UG_INITIAL_STAGED);
    ARM_REFUSES(initial_value.phase=UG_INITIAL_TERMINAL);
    ARM_REFUSES(status_value.faults=UG_BAD_PROBE);
    ARM_REFUSES(status_value.first_outcome=UG_EVENT_DENIAL);
#undef ARM_REFUSES
    probe_fresh();assert(ug_session_probe_arm(&subject,2,2,UG_PROBE_POLL,&receipt)==-1);
    probe_sticky();passed++;
    probe_fresh();assert(ug_session_probe_arm(&subject,2,3,3,&receipt)==-1);
    probe_sticky();passed++;
    probe_fresh();probe_present=true;
    assert(ug_session_probe_arm(&subject,2,3,UG_PROBE_POLL,&receipt)==-1 && errno==EEXIST);
    assert(probe_present && subject.initial[0].probe_phase==UG_PROBE_ARMED);probe_sticky();passed++;
    for(int fault=1;fault<=2;fault++) {
        probe_fresh();probe_update_failure=fault;
        assert(ug_session_probe_arm(&subject,2,3,UG_PROBE_POLL,&receipt)==-1 && errno==EIO);
        assert(subject.initial[0].probe_phase==UG_PROBE_ARMED && probe_present==(fault==2));
        probe_update_failure=0;probe_sticky();passed++;
    }
    probe_fresh();probe_read_failure=1;
    assert(ug_session_probe_arm(&subject,2,3,UG_PROBE_POLL,&receipt)==-1 && errno==EIO);
    assert(probe_present);probe_read_failure=0;probe_sticky();passed++;

#define SUBMIT_REFUSES(change) do { probe_fresh();probe_arm(UG_PROBE_POLL);change;     assert(ug_session_probe_submit(&subject,2,3,&receipt)==-1);     probe_sticky();assert(probe_updates==1);passed++; } while(0)
    SUBMIT_REFUSES(probe_present=false);
    SUBMIT_REFUSES(probe_value.incarnation=8);
    SUBMIT_REFUSES(probe_value.sequence=4);
    SUBMIT_REFUSES(probe_value.phase=UG_PROBE_COMPLETED);
    SUBMIT_REFUSES(probe_value.observations=1);
    SUBMIT_REFUSES(probe_value.denied=1);
#undef SUBMIT_REFUSES
    probe_fresh();probe_arm(UG_PROBE_POLL);probe_submit();
    assert(ug_session_probe_submit(&subject,2,3,&receipt)==-1);probe_sticky();passed++;
    probe_fresh();probe_arm(UG_PROBE_POLL);
    assert(ug_session_probe_arm(&subject,2,4,UG_PROBE_POLL,&receipt)==-1);probe_sticky();passed++;
    probe_fresh();probe_arm(UG_PROBE_POLL);
    assert(ug_session_probe_submit(&subject,2,4,&receipt)==-1);probe_sticky();passed++;

#define COMPLETE_REFUSES(change,raw) do { probe_fresh();probe_arm(UG_PROBE_POLL);probe_submit();     probe_value.observations=1;change;     assert(ug_session_probe_complete(&subject,2,3,raw,&receipt)==-1);     probe_sticky();assert(probe_updates==2 && !probe_deletes);passed++; } while(0)
    COMPLETE_REFUSES(probe_value.observations=0,0);
    COMPLETE_REFUSES(probe_value.observations=2,0);
    COMPLETE_REFUSES(probe_value.observations=UINT64_MAX,0);
    COMPLETE_REFUSES(probe_value.denied=1,0);
    COMPLETE_REFUSES(probe_value.sequence=4,0);
    COMPLETE_REFUSES(probe_value.phase=UG_PROBE_ARMED,0);
    COMPLETE_REFUSES(probe_present=false,0);
    COMPLETE_REFUSES(status_value.faults=UG_BAD_PROBE,0);
    COMPLETE_REFUSES((void)0,2);
    COMPLETE_REFUSES((void)0,(u64)(int64_t)-514);
    COMPLETE_REFUSES((void)0,(u64)(int64_t)-EINTR);
    COMPLETE_REFUSES((void)0,(u64)(int64_t)-EFAULT);
    COMPLETE_REFUSES((void)0,(u64)(int64_t)-512);
#undef COMPLETE_REFUSES
    probe_fresh();probe_arm(UG_PROBE_PPOLL);probe_submit();probe_value.observations=1;
    assert(ug_session_probe_complete(&subject,2,3,(u64)(int64_t)-516,&receipt)==-1);probe_sticky();passed++;
    probe_fresh();probe_arm(UG_PROBE_POLL);probe_submit();probe_complete(0);
    assert(ug_session_probe_complete(&subject,2,3,0,&receipt)==-1);probe_sticky();passed++;
    probe_fresh();probe_arm(UG_PROBE_POLL);probe_submit();
    assert(ug_session_probe_retire(&subject,2,3,UG_PROBE_RETIRE_UNENTERED,&receipt)==-1);
    assert(probe_present && !probe_deletes);probe_sticky();passed++;
    probe_fresh();probe_arm(UG_PROBE_POLL);
    assert(ug_session_probe_retire(&subject,2,3,UG_PROBE_RETIRE_COMPLETED,&receipt)==-1);
    assert(probe_present && !probe_deletes);probe_sticky();passed++;
    probe_fresh();probe_arm(UG_PROBE_POLL);probe_submit();probe_complete(0);
    assert(ug_session_probe_retire(&subject,2,3,0,&receipt)==-1);probe_sticky();passed++;
    for(int fault=1;fault<=3;fault++) {
        probe_fresh();probe_arm(UG_PROBE_POLL);probe_submit();probe_complete(0);probe_delete_failure=fault;
        assert(ug_session_probe_retire(&subject,2,3,UG_PROBE_RETIRE_COMPLETED,&receipt)==-1);
        assert(subject.initial[0].probe_phase==UG_PROBE_COMPLETED);
        probe_delete_failure=0;probe_sticky();passed++;
    }
    for(int phase=UG_PROBE_SUBMITTED;phase<=UG_PROBE_COMPLETED;phase++)
        for(int fault=1;fault<=2;fault++) {
            probe_fresh();probe_arm(UG_PROBE_POLL);
            if(phase==UG_PROBE_COMPLETED) {probe_submit();probe_value.observations=1;}
            probe_update_failure=fault;
            int result=phase==UG_PROBE_SUBMITTED?
                ug_session_probe_submit(&subject,2,3,&receipt):
                ug_session_probe_complete(&subject,2,3,0,&receipt);
            assert(result==-1 && errno==EIO && subject.initial[0].probe_phase==(u64)phase);
            assert(probe_present && probe_value.phase==(u64)(fault==2?phase:phase-1));
            probe_update_failure=0;probe_sticky();passed++;
        }
    for(int phase=UG_PROBE_SUBMITTED;phase<=UG_PROBE_COMPLETED;phase++) {
        probe_fresh();probe_arm(UG_PROBE_POLL);
        if(phase==UG_PROBE_COMPLETED) {probe_submit();probe_value.observations=1;}
        probe_read_fail_at=probe_reads+2; /* Successful update, unreadable readback. */
        int result=phase==UG_PROBE_SUBMITTED?
            ug_session_probe_submit(&subject,2,3,&receipt):
            ug_session_probe_complete(&subject,2,3,0,&receipt);
        assert(result==-1 && errno==EIO && probe_value.phase==(u64)phase);
        probe_sticky();passed++;
    }
    probe_fresh();probe_arm(UG_PROBE_POLL);probe_submit();probe_complete(1);
    probe_read_fail_at=probe_reads+2; /* Successful delete, uncertain absence. */
    assert(ug_session_probe_retire(&subject,2,3,UG_PROBE_RETIRE_COMPLETED,&receipt)==-1);
    assert(!probe_present && subject.initial[0].probe_phase==UG_PROBE_COMPLETED);
    probe_sticky();passed++;
    probe_fresh();probe_arm(UG_PROBE_POLL);probe_submit();probe_complete(1);
    assert(ug_session_probe_complete(&subject,2,4,0,&receipt)==-1);
    assert(subject.initial[0].probe_raw==1);probe_sticky();passed++;
    probe_fresh();probe_arm(UG_PROBE_POLL);probe_submit();probe_complete(1);
    probe_value.observations=2;
    assert(ug_session_probe_retire(&subject,2,3,UG_PROBE_RETIRE_COMPLETED,&receipt)==-1);
    assert(probe_present && !probe_deletes);probe_sticky();passed++;
    probe_control=false;
    printf("guard_probe_session_controls=%u passed; native operations substituted\n",passed);
    assert(passed==79);
}
int main(void) {
    assert(retained_terminal_controls()==0);unsigned passed=0;
    struct ug_terminal_receipt proof=prepare();struct ug_object_close closed;
    assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal,4000000000,&closed)==0);
    assert(object_closed && !dir_present && queries==0 && closed.count==72);
    assert(closed.closed_ns==1000000000 && closed.deadline_ns==2000000000);
    assert(closed.record_ordinal>proof.record_ordinal);passed++;
    proof=prepare();assert(ug_session_close_terminal(&subject,11,9,proof.record_ordinal,4000000000,&closed)==-1 && errno==EINVAL);
    assert(!object_closed && !subject.close_submitted);passed++;
    proof=prepare();assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal+1,4000000000,&closed)==-1 && errno==EINVAL);
    assert(!object_closed);passed++;
    proof=prepare();assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal,clock_ns,&closed)==-1 && errno==ETIMEDOUT);
    assert(!object_closed);passed++;
    proof=prepare();assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal,1500000000,&closed)==0);
    assert(closed.deadline_ns==1500000000);passed++;
    proof=prepare();clock_failure=2;
    assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal,4000000000,&closed)==-1 && errno==EIO);
    assert(object_closed && subject.close_submitted);clock_failure=0;
    assert(ug_session_close_terminal(&subject,12,10,proof.record_ordinal,9000000000,&closed)==-1 && errno==EIO);passed++;
    proof=prepare();subject.journal_error=EIO;
    assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal,4000000000,&closed)==-1 && errno==EIO);
    assert(!object_closed);passed++;
    proof=prepare();assert(ug_session_close_terminal(&subject,11,10,proof.record_ordinal,4000000000,&closed)==0);
    assert(ug_session_close_terminal(&subject,12,10,proof.record_ordinal,9000000000,&closed)==-1 && errno==EBUSY);passed++;
    printf("guard_two_phase_controls=%u passed\n",passed);assert(passed==8);
    provenance_controls();probe_controls();return 0;
}
