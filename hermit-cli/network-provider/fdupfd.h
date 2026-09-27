/* SPDX-License-Identifier: MIT */
#ifndef HERMIT_PROVIDER_FDUPFD_H
#define HERMIT_PROVIDER_FDUPFD_H
#include "fd-effects.h"
#include "retirement-target.h"
/* Private state inside the existing fd_install_calls row. It owns no kernel
 * reference and is never wire authority. The original kernel caller retains
 * file custody through f_dupfd; only scalars are read after installation. */
struct ap_fdupfd {
    u64 function_ip, entry_stack, allocation_stack, raw_table;
    u32 minimum, flags, phase;
    s32 allocated;
};
static __attribute__((always_inline)) inline int ap_fdupfd_allocation_enter(
    struct ap_fdupfd *s,u64 return_ip,u64 stack,u64 raw_table,u32 minimum,u32 flags) {
    if(!s || !s->entry_stack || !stack || !raw_table || s->raw_table!=raw_table ||
       s->phase || s->allocation_stack || s->minimum!=minimum || s->flags!=flags ||
       !ap_fdupfd_alloc_site(s->function_ip,return_ip))return 0;
    s->allocation_stack=stack;s->phase=1;return 1;
}
static __attribute__((always_inline)) inline int ap_fdupfd_allocation_return(
    struct ap_fdupfd *s,u64 cookie,u64 raw_table,s32 returned) {
    if(!s || !cookie || s->phase!=1 || s->allocation_stack!=cookie ||
       !raw_table || s->raw_table!=raw_table || returned < -4095 ||
       (returned>=0 && (u32)returned<s->minimum))return 0;
    s->allocated=returned;s->phase=2;return 1;
}
/* -1 is incomplete/contradictory; 0 proves this invocation did not install;
 * 1 closes an already emitted positive allocation interval. This is never a
 * lookup of the descriptor after a concurrent task may have removed/reused it. */
static __attribute__((always_inline)) inline int ap_fdupfd_completion(
    const struct ap_fdupfd *s,u64 cookie,u64 raw_table,s32 returned,u64 begin) {
    if(!s || !cookie || s->entry_stack!=cookie || !s->function_ip ||
       !raw_table || s->raw_table!=raw_table)return -1;
    if(!s->phase && !s->allocation_stack && !begin)
        return returned==-22?0:-1; /* audited pre-allocation RLIMIT rejection */
    if(s->phase!=2 || !s->allocation_stack || returned!=s->allocated)return -1;
    if(returned<0)return !begin?0:-1;
    return begin?1:-1;
}
#endif
