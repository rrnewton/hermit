/* SPDX-License-Identifier: MIT */
#include <assert.h>
#include <stdio.h>
#include <linux/bpf.h>
#include "ftrace-coverage.h"

int main(void) {
    assert(ap_ftrace_coverage_mask()==AP_FTRACE_ALL_ROLES);
    for(unsigned role=1;role<=17;role++)assert(ap_ftrace_role_enabled(role));
    assert(!ap_ftrace_role_enabled(0));
    assert(!ap_ftrace_role_enabled(18));
    assert(ap_ftrace_link_type_allowed(BPF_LINK_TYPE_TRACING,BPF_LINK_TYPE_PERF_EVENT));
    assert(ap_ftrace_link_type_allowed(BPF_LINK_TYPE_RAW_TRACEPOINT,BPF_LINK_TYPE_PERF_EVENT));
    assert(ap_ftrace_link_type_allowed(BPF_LINK_TYPE_KPROBE_MULTI,BPF_LINK_TYPE_PERF_EVENT));
    assert(!ap_ftrace_link_type_allowed(BPF_LINK_TYPE_PERF_EVENT,BPF_LINK_TYPE_PERF_EVENT));
    assert(!ap_ftrace_link_type_allowed(BPF_LINK_TYPE_CGROUP,BPF_LINK_TYPE_PERF_EVENT));
    assert(!ap_ftrace_link_type_allowed(0,BPF_LINK_TYPE_PERF_EVENT));
    assert(ap_ftrace_program_link_pair_allowed(BPF_PROG_TYPE_TRACING,
        BPF_LINK_TYPE_TRACING,BPF_LINK_TYPE_PERF_EVENT));
    assert(ap_ftrace_program_link_pair_allowed(BPF_PROG_TYPE_TRACING,
        BPF_LINK_TYPE_RAW_TRACEPOINT,BPF_LINK_TYPE_PERF_EVENT));
    assert(ap_ftrace_program_link_pair_allowed(BPF_PROG_TYPE_KPROBE,
        BPF_LINK_TYPE_KPROBE_MULTI,BPF_LINK_TYPE_PERF_EVENT));
    assert(!ap_ftrace_program_link_pair_allowed(BPF_PROG_TYPE_KPROBE,
        BPF_LINK_TYPE_PERF_EVENT,BPF_LINK_TYPE_PERF_EVENT));
    assert(!ap_ftrace_program_link_pair_allowed(BPF_PROG_TYPE_TRACING,
        BPF_LINK_TYPE_KPROBE_MULTI,BPF_LINK_TYPE_PERF_EVENT));
    assert(!ap_ftrace_program_link_pair_allowed(BPF_PROG_TYPE_KPROBE,
        BPF_LINK_TYPE_RAW_TRACEPOINT,BPF_LINK_TYPE_PERF_EVENT));
    uint64_t addresses[]={0x3000,0x1000,0x2000},cookies[]={30,10,20};
    const uint64_t expected_addresses[]={0x1000,0x2000,0x3000};
    const uint64_t expected_cookies[]={10,20,30};
    struct bpf_prog_info multi_program={.type=BPF_PROG_TYPE_KPROBE,.id=31};
    struct bpf_link_info multi_link={.type=BPF_LINK_TYPE_KPROBE_MULTI,.id=33,.prog_id=31};
    multi_link.kprobe_multi.count=3;
    assert(ap_ftrace_kprobe_multi_link_matches(&multi_program,sizeof(multi_program),
        &multi_link,sizeof(multi_link),addresses,cookies,expected_addresses,expected_cookies,
        3,0));
    multi_link.kprobe_multi.flags=BPF_F_KPROBE_MULTI_RETURN;
    assert(ap_ftrace_kprobe_multi_link_matches(&multi_program,sizeof(multi_program),
        &multi_link,sizeof(multi_link),addresses,cookies,expected_addresses,expected_cookies,
        3,BPF_F_KPROBE_MULTI_RETURN));
    multi_link.kprobe_multi.flags=0;
    struct bpf_prog_info good_multi_program=multi_program;
    struct bpf_link_info good_multi_link=multi_link;
    assert(ap_ftrace_task_scoped_kprobe_multi_link_matches(&good_multi_program,
        sizeof(good_multi_program),&good_multi_link,sizeof(good_multi_link),addresses,cookies,
        expected_addresses,expected_cookies,3,0));
#define BAD_MULTI_PROGRAM(field,value) do {multi_program=good_multi_program;multi_program.field=value; \
    assert(!ap_ftrace_kprobe_multi_link_matches(&multi_program,sizeof(multi_program), \
        &good_multi_link,sizeof(good_multi_link),addresses,cookies,expected_addresses,expected_cookies, \
        3,0));} while(0)
#define BAD_MULTI_LINK(field,value) do {multi_link=good_multi_link;multi_link.field=value; \
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program), \
        &multi_link,sizeof(multi_link),addresses,cookies,expected_addresses,expected_cookies, \
        3,0));} while(0)
    BAD_MULTI_PROGRAM(type,BPF_PROG_TYPE_TRACING);BAD_MULTI_PROGRAM(id,0);
    BAD_MULTI_PROGRAM(recursion_misses,1);
    BAD_MULTI_LINK(type,BPF_LINK_TYPE_TRACING);
    BAD_MULTI_LINK(id,0);BAD_MULTI_LINK(prog_id,32);BAD_MULTI_LINK(kprobe_multi.count,2);
    BAD_MULTI_LINK(kprobe_multi.flags,BPF_F_KPROBE_MULTI_RETURN);
    BAD_MULTI_LINK(kprobe_multi.missed,1);
    multi_program=good_multi_program;multi_program.recursion_misses=1;
    assert(ap_ftrace_task_scoped_kprobe_multi_link_matches(&multi_program,sizeof(multi_program),
        &good_multi_link,sizeof(good_multi_link),addresses,cookies,
        expected_addresses,expected_cookies,3,0));
    multi_link=good_multi_link;multi_link.kprobe_multi.missed=1;
    assert(ap_ftrace_task_scoped_kprobe_multi_link_matches(&good_multi_program,
        sizeof(good_multi_program),&multi_link,sizeof(multi_link),addresses,cookies,
        expected_addresses,expected_cookies,3,0));
    multi_program.recursion_misses=1;
    assert(ap_ftrace_task_scoped_kprobe_multi_link_matches(&multi_program,sizeof(multi_program),
        &multi_link,sizeof(multi_link),addresses,cookies,
        expected_addresses,expected_cookies,3,0));
#define BAD_SCOPED_PROGRAM(field,value) do {multi_program=good_multi_program;multi_program.field=value; \
    assert(!ap_ftrace_task_scoped_kprobe_multi_link_matches(&multi_program,sizeof(multi_program), \
        &good_multi_link,sizeof(good_multi_link),addresses,cookies,expected_addresses,expected_cookies, \
        3,0));} while(0)
#define BAD_SCOPED_LINK(field,value) do {multi_link=good_multi_link;multi_link.field=value; \
    assert(!ap_ftrace_task_scoped_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program), \
        &multi_link,sizeof(multi_link),addresses,cookies,expected_addresses,expected_cookies, \
        3,0));} while(0)
    BAD_SCOPED_PROGRAM(type,BPF_PROG_TYPE_TRACING);BAD_SCOPED_PROGRAM(id,0);
    BAD_SCOPED_LINK(type,BPF_LINK_TYPE_TRACING);BAD_SCOPED_LINK(id,0);
    BAD_SCOPED_LINK(prog_id,32);BAD_SCOPED_LINK(kprobe_multi.count,2);
    BAD_SCOPED_LINK(kprobe_multi.flags,BPF_F_KPROBE_MULTI_RETURN);
#undef BAD_SCOPED_LINK
#undef BAD_SCOPED_PROGRAM
    uint64_t changed_addresses[]={0x3000,0x1001,0x2000};
    uint64_t changed_cookies[]={30,11,20};
    uint64_t duplicate_addresses[]={0x3000,0x1000,0x1000};
    uint64_t duplicate_cookies[]={30,10,10};
    uint64_t zero_expected_addresses[]={0,0x2000,0x3000};
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        &good_multi_link,sizeof(good_multi_link),changed_addresses,cookies,
        expected_addresses,expected_cookies,3,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        &good_multi_link,sizeof(good_multi_link),addresses,changed_cookies,
        expected_addresses,expected_cookies,3,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        &good_multi_link,sizeof(good_multi_link),duplicate_addresses,duplicate_cookies,
        expected_addresses,expected_cookies,3,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        &good_multi_link,sizeof(good_multi_link),addresses,cookies,
        zero_expected_addresses,expected_cookies,3,0));
    assert(!ap_ftrace_task_scoped_kprobe_multi_link_matches(&good_multi_program,
        sizeof(good_multi_program),&good_multi_link,sizeof(good_multi_link),changed_addresses,cookies,
        expected_addresses,expected_cookies,3,0));
    assert(!ap_ftrace_task_scoped_kprobe_multi_link_matches(&good_multi_program,
        sizeof(good_multi_program),&good_multi_link,sizeof(good_multi_link),addresses,changed_cookies,
        expected_addresses,expected_cookies,3,0));
    assert(!ap_ftrace_task_scoped_kprobe_multi_link_matches(&good_multi_program,
        sizeof(good_multi_program),&good_multi_link,sizeof(good_multi_link),duplicate_addresses,
        duplicate_cookies,expected_addresses,expected_cookies,3,0));
    assert(!ap_ftrace_task_scoped_kprobe_multi_link_matches(&good_multi_program,
        sizeof(good_multi_program),&good_multi_link,sizeof(good_multi_link),addresses,cookies,
        zero_expected_addresses,expected_cookies,3,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,0,
        &good_multi_link,sizeof(good_multi_link),addresses,cookies,
        expected_addresses,expected_cookies,3,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        &good_multi_link,0,addresses,cookies,expected_addresses,expected_cookies,
        3,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        &good_multi_link,offsetof(struct bpf_link_info,kprobe_multi.cookies),
        addresses,cookies,expected_addresses,expected_cookies,3,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(0,sizeof(good_multi_program),
        &good_multi_link,sizeof(good_multi_link),addresses,cookies,
        expected_addresses,expected_cookies,3,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        0,sizeof(good_multi_link),addresses,cookies,expected_addresses,expected_cookies,
        3,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        &good_multi_link,sizeof(good_multi_link),0,cookies,expected_addresses,expected_cookies,
        3,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        &good_multi_link,sizeof(good_multi_link),addresses,0,expected_addresses,expected_cookies,
        3,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        &good_multi_link,sizeof(good_multi_link),addresses,cookies,0,expected_cookies,
        3,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        &good_multi_link,sizeof(good_multi_link),addresses,cookies,expected_addresses,0,
        3,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        &good_multi_link,sizeof(good_multi_link),addresses,cookies,
        expected_addresses,expected_cookies,0,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        &good_multi_link,sizeof(good_multi_link),addresses,cookies,
        expected_addresses,expected_cookies,65,0));
    assert(!ap_ftrace_kprobe_multi_link_matches(&good_multi_program,sizeof(good_multi_program),
        &good_multi_link,sizeof(good_multi_link),addresses,cookies,
        expected_addresses,expected_cookies,3,2));
#undef BAD_MULTI_LINK
#undef BAD_MULTI_PROGRAM
    struct bpf_prog_info program={.type=BPF_PROG_TYPE_TRACING,.id=37,
        .attach_btf_id=41,.attach_btf_obj_id=43};
    struct bpf_link_info link={.type=BPF_LINK_TYPE_TRACING,.id=47,.prog_id=37};
    link.tracing.attach_type=BPF_TRACE_FENTRY;
    link.tracing.target_obj_id=43;link.tracing.target_btf_id=41;
    assert(ap_ftrace_read_link_matches(&program,sizeof(program),&link,sizeof(link)));
    struct bpf_prog_info good_program=program;struct bpf_link_info good_link=link;
#define BAD_PROGRAM(field,value) do {program=good_program;program.field=value; \
    assert(!ap_ftrace_read_link_matches(&program,sizeof(program),&good_link,sizeof(good_link)));} while(0)
#define BAD_LINK(field,value) do {link=good_link;link.field=value; \
    assert(!ap_ftrace_read_link_matches(&good_program,sizeof(good_program),&link,sizeof(link)));} while(0)
    BAD_PROGRAM(type,BPF_PROG_TYPE_KPROBE);BAD_PROGRAM(id,0);BAD_PROGRAM(recursion_misses,1);
    BAD_PROGRAM(attach_btf_id,0);BAD_PROGRAM(attach_btf_obj_id,44);
    BAD_LINK(type,BPF_LINK_TYPE_PERF_EVENT);BAD_LINK(id,0);BAD_LINK(prog_id,38);
    BAD_LINK(tracing.attach_type,BPF_TRACE_FEXIT);BAD_LINK(tracing.target_obj_id,44);
    BAD_LINK(tracing.target_btf_id,42);BAD_LINK(tracing.cookie,1);
    assert(!ap_ftrace_read_link_matches(&good_program,0,&good_link,sizeof(good_link)));
    assert(!ap_ftrace_read_link_matches(&good_program,sizeof(good_program),&good_link,0));
    assert(!ap_ftrace_read_link_matches(0,sizeof(good_program),&good_link,sizeof(good_link)));
    assert(!ap_ftrace_read_link_matches(&good_program,sizeof(good_program),0,sizeof(good_link)));
#undef BAD_LINK
#undef BAD_PROGRAM
    printf("ftrace replacement coverage: all 17 old roles\n");
    return 0;
}
