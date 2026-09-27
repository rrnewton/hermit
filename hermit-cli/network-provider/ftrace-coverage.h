/* SPDX-License-Identifier: MIT */
#ifndef HERMIT_FTRACE_COVERAGE_H
#define HERMIT_FTRACE_COVERAGE_H
#ifndef AP_FTRACE_MUTATE_ROLE
#define AP_FTRACE_MUTATE_ROLE 0U
#endif
#ifndef AP_FTRACE_MUTATE_LINK_SHAPE
#define AP_FTRACE_MUTATE_LINK_SHAPE 0
#endif
_Static_assert(AP_FTRACE_MUTATE_ROLE<=17U,"ftrace role mutant");
#define AP_FTRACE_ALL_ROLES ((1U<<17)-1U)
static __attribute__((always_inline)) inline int ap_ftrace_role_enabled(unsigned role) {
    return role>=1 && role<=17 && role!=AP_FTRACE_MUTATE_ROLE;
}
static __attribute__((always_inline)) inline unsigned ap_ftrace_coverage_mask(void) {
    return AP_FTRACE_ALL_ROLES &
        (AP_FTRACE_MUTATE_ROLE ? ~(1U<<((AP_FTRACE_MUTATE_ROLE+16U)%17U)) : ~0U);
}
#ifndef __BPF__
#include <stddef.h>
#include <stdint.h>
#include <linux/bpf.h>
static inline int ap_ftrace_link_type_allowed(unsigned type,unsigned perf_event_type) {
    return type==BPF_LINK_TYPE_TRACING || type==BPF_LINK_TYPE_RAW_TRACEPOINT ||
        type==BPF_LINK_TYPE_KPROBE_MULTI ||
        (AP_FTRACE_MUTATE_LINK_SHAPE && type==perf_event_type);
}
static inline int ap_ftrace_program_link_pair_allowed(
        unsigned program_type,unsigned link_type,unsigned perf_event_type) {
    return (program_type==BPF_PROG_TYPE_TRACING &&
            (link_type==BPF_LINK_TYPE_TRACING || link_type==BPF_LINK_TYPE_RAW_TRACEPOINT)) ||
        (program_type==BPF_PROG_TYPE_KPROBE &&
         (link_type==BPF_LINK_TYPE_KPROBE_MULTI ||
          (AP_FTRACE_MUTATE_LINK_SHAPE && link_type==perf_event_type)));
}
static inline int ap_ftrace_kprobe_multi_link_matches_mode(
        const struct bpf_prog_info *program,unsigned program_size,
        const struct bpf_link_info *link,unsigned link_size,
        const uint64_t *addresses,const uint64_t *cookies,
        const uint64_t *expected_addresses,const uint64_t *expected_cookies,
        unsigned count,unsigned flags,int require_clean_counters) {
    if(!program || !link || !addresses || !cookies || !expected_addresses ||
       !expected_cookies || !count || count>64 ||
       (flags&~BPF_F_KPROBE_MULTI_RETURN) ||
       program_size<offsetof(struct bpf_prog_info,recursion_misses)+sizeof(program->recursion_misses) ||
       link_size<offsetof(struct bpf_link_info,kprobe_multi.cookies)+sizeof(link->kprobe_multi.cookies) ||
       program->type!=BPF_PROG_TYPE_KPROBE || !program->id ||
       (require_clean_counters && program->recursion_misses) ||
       link->type!=BPF_LINK_TYPE_KPROBE_MULTI || !link->id || link->prog_id!=program->id ||
       link->kprobe_multi.count!=count || link->kprobe_multi.flags!=flags ||
       (require_clean_counters && link->kprobe_multi.missed))return 0;
    uint64_t seen=0;
    for(unsigned actual=0;actual<count;actual++) {
        unsigned expected=0;
        while(expected<count && (addresses[actual]!=expected_addresses[expected] ||
              cookies[actual]!=expected_cookies[expected]))expected++;
        if(expected==count || !expected_addresses[expected] || (seen&(UINT64_C(1)<<expected)))
            return 0;
        seen|=UINT64_C(1)<<expected;
    }
    return seen==(count==64?UINT64_MAX:(UINT64_C(1)<<count)-1);
}
static inline int ap_ftrace_kprobe_multi_link_matches(
        const struct bpf_prog_info *program,unsigned program_size,
        const struct bpf_link_info *link,unsigned link_size,
        const uint64_t *addresses,const uint64_t *cookies,
        const uint64_t *expected_addresses,const uint64_t *expected_cookies,
        unsigned count,unsigned flags) {
    return ap_ftrace_kprobe_multi_link_matches_mode(program,program_size,link,link_size,
        addresses,cookies,expected_addresses,expected_cookies,count,flags,1);
}
static inline int ap_ftrace_task_scoped_kprobe_multi_link_matches(
        const struct bpf_prog_info *program,unsigned program_size,
        const struct bpf_link_info *link,unsigned link_size,
        const uint64_t *addresses,const uint64_t *cookies,
        const uint64_t *expected_addresses,const uint64_t *expected_cookies,
        unsigned count,unsigned flags) {
    return ap_ftrace_kprobe_multi_link_matches_mode(program,program_size,link,link_size,
        addresses,cookies,expected_addresses,expected_cookies,count,flags,0);
}
static inline int ap_ftrace_read_link_matches(
        const struct bpf_prog_info *program,unsigned program_size,
        const struct bpf_link_info *link,unsigned link_size) {
    return program && link &&
        program_size>=offsetof(struct bpf_prog_info,attach_btf_id)+sizeof(program->attach_btf_id) &&
        link_size>=offsetof(struct bpf_link_info,tracing.cookie)+sizeof(link->tracing.cookie) &&
        program->type==BPF_PROG_TYPE_TRACING && program->id && !program->recursion_misses &&
        program->attach_btf_id && link->type==BPF_LINK_TYPE_TRACING && link->id &&
        link->prog_id==program->id && link->tracing.attach_type==BPF_TRACE_FENTRY &&
        link->tracing.target_obj_id==program->attach_btf_obj_id &&
        link->tracing.target_btf_id==program->attach_btf_id && !link->tracing.cookie;
}
#endif
#endif
