/* SPDX-License-Identifier: MIT */
/* Actual shared journal producer, with only kernel boundary functions replaced.
 * These controls perform no map, BPF, tracefs or native-provider operation. */
#include <assert.h>
#include <stdio.h>
#include <string.h>
#include "fd-effects.h"
#define INLINE static __attribute__((always_inline))
#define CORE(value) (value)
#define BPF_NOEXIST 1ULL

static struct ap_fd_status status;
static struct ap_fd_event row,expected;
static unsigned stats_calls,update_calls,lookup_calls,problem_calls,publish_calls;
static unsigned fail_stats,fail_update,fail_lookup,changed_completion;
static int fd_journal;
static struct {u64 start_boottime;} actor;
static struct ap_fd_status *fd_stats(void) {
    stats_calls++;return fail_stats?NULL:&status;
}
static void fd_problem(u64 problem) {problem_calls++;status.problem|=problem;}
static long update(void *map,const void *key,const void *value,u64 flags) {
    update_calls++;
    assert(map==&fd_journal && flags==BPF_NOEXIST);
    const u64 sequence=*(const u64 *)key;
    const struct ap_fd_event pending={.sequence=sequence,.complete=2};
    assert(sequence==expected.sequence);
    assert(!memcmp(value,&pending,sizeof(pending)));
    if(fail_update)return -5;
    row=*(const struct ap_fd_event *)value;return 0;
}
static void *lookup(void *map,const void *key) {
    lookup_calls++;assert(map==&fd_journal);
    assert(*(const u64 *)key==expected.sequence);
    return fail_lookup?NULL:&row;
}
static u64 pid_tgid(void) {return expected.task;}
static typeof(actor) *current_task(void) {return &actor;}
static u64 compare_completion(u64 *at,u64 before,u64 after) {
    publish_calls++;assert(at==&row.complete && before==2 && after==1);
    /* Every actual payload field must precede publication. */
    assert(!memcmp(&row,&expected,sizeof(row)));
    if(changed_completion)row.complete=9;
    const u64 found=*at;
    if(found==before)*at=after;
    return found;
}
#define __sync_val_compare_and_swap(p,a,b) compare_completion((p),(a),(b))
#include "fd-journal.bpf.h"
#undef __sync_val_compare_and_swap

static void reset(void) {
    status=(struct ap_fd_status){.next_event=41};
    row=(struct ap_fd_event){0};
    expected=(struct ap_fd_event){.sequence=42,.complete=2,.kind=1,
        .task=3,.task_start=5,.table=7,.file=11,.previous_file=13,
        .dependency=17,.accept_command=19,.fd=-23,.returned=-29,
        .mode=31,.status_flags=37,.device_major=43,.device_minor=47};
    actor.start_boottime=expected.task_start;
    stats_calls=update_calls=lookup_calls=problem_calls=publish_calls=0;
    fail_stats=fail_update=fail_lookup=changed_completion=0;
}
static u64 publish(void) {
    return fd_event_for_profile(expected.task,expected.task_start,expected.kind,
        expected.table,expected.fd,expected.file,expected.previous_file,
        expected.dependency,expected.accept_command,expected.returned,
        expected.mode,expected.status_flags,expected.device_major,expected.device_minor);
}
static void published(void) {
    assert(stats_calls==1 && update_calls==1 && lookup_calls==1 && publish_calls==1);
    assert(!problem_calls && !status.problem && status.next_event==expected.sequence);
    expected.complete=1;assert(!memcmp(&row,&expected,sizeof(row)));
}
static void all_fields_and_wrappers(void) {
    reset();assert(publish()==42);published();
    reset();expected.mode=expected.status_flags=expected.device_major=expected.device_minor=0;
    assert(fd_event_for(expected.task,expected.task_start,expected.kind,expected.table,
        expected.fd,expected.file,expected.previous_file,expected.dependency,
        expected.accept_command,expected.returned)==42);published();
    reset();expected.mode=expected.status_flags=expected.device_major=expected.device_minor=0;
    assert(fd_event(expected.kind,expected.table,expected.fd,expected.file,
        expected.previous_file,expected.dependency,expected.accept_command,expected.returned)==42);
    published();
    /* Full-width varied values distinguish every field from every neighbour;
     * signed descriptor/return and four packed u32 fields remain exact. */
    for(unsigned i=0;i<256;i++) {
        reset();
        expected.task=0x8000000000000000ULL+(u64)i;
        expected.task_start=~0ULL-i;
        expected.kind=0x100000000ULL+i*3;
        expected.table=0x200000000ULL+i*5;
        expected.file=0x400000000ULL+i*7;
        expected.previous_file=0x800000000ULL+i*11;
        expected.dependency=0x1000000000ULL+i*13;
        expected.accept_command=0x2000000000ULL+i*17;
        expected.fd=(s32)(0x80000000U+i*19);
        expected.returned=(s32)(0x7fffffffU-i*23);
        expected.mode=0x80000000U+i*29;
        expected.status_flags=0x40000000U+i*31;
        expected.device_major=0x20000000U+i*37;
        expected.device_minor=0x10000000U+i*41;
        assert(publish()==42);published();
    }
}
static void refusal_and_publication_order(void) {
    reset();fail_stats=1;
    assert(!publish());assert(stats_calls==1 && !update_calls && !lookup_calls && !publish_calls);
    assert(!problem_calls && !status.problem && status.next_event==41);
    reset();status.next_event=~0ULL;
    assert(!publish());assert(stats_calls==1 && !update_calls && !lookup_calls && !publish_calls);
    assert(problem_calls==1 && status.problem==AP_FD_CAPACITY && !status.next_event);
    reset();fail_update=1;
    assert(!publish());assert(stats_calls==1 && update_calls==1 && !lookup_calls && !publish_calls);
    assert(problem_calls==1 && status.problem==AP_FD_CAPACITY && status.next_event==42);
    const struct ap_fd_event empty={0};assert(!memcmp(&row,&empty,sizeof(row)));
    reset();fail_lookup=1;
    assert(!publish());assert(stats_calls==1 && update_calls==1 && lookup_calls==1 && !publish_calls);
    assert(problem_calls==1 && status.problem==AP_FD_MISSING && status.next_event==42);
    const struct ap_fd_event pending={.sequence=42,.complete=2};
    assert(!memcmp(&row,&pending,sizeof(row)));
    /* Preserve the old CAS semantics: an unexpected completion is not silently
     * overwritten. Returning the reserved sequence does not make this row DONE. */
    reset();changed_completion=1;
    assert(publish()==42 && publish_calls==1);
    expected.complete=9;assert(!memcmp(&row,&expected,sizeof(row)));
    assert(!problem_calls && !status.problem);
}
int main(void) {
    all_fields_and_wrappers();refusal_and_publication_order();
    puts("journal actual publisher:259 complete payloads/wrappers,4 reservation refusals,final CAS unchanged-state refusal");
    return 0;
}
