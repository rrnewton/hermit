/* SPDX-License-Identifier: GPL-2.0 */
#ifndef HERMIT_FD_CALL_SHARED_BPF_H
#define HERMIT_FD_CALL_SHARED_BPF_H
/* Each invocation writes the same four-word caller-owned key freshly, then
 * performs the same fd_calls lookup. No identity cache, additional lookup,
 * different map or command admission is introduced. */
#ifdef __BPF__
static __attribute__((noinline)) struct ap_fd_call *
#else
static __attribute__((always_inline)) inline struct ap_fd_call *
#endif
fd_actor_call(struct ap_invocation_key *key) {
    *key=fd_actor();return lookup(&fd_calls,key);
}
/* Callers have already validated the exact physical selection and still hold
 * its borrowed kernel file reference. Accept keeps its separate problem cell
 * and has no original-selection publication. All other callers retain the
 * existing final CAS after both selection fields are stored. */
#ifdef __BPF__
static __attribute__((noinline)) void
#else
static __attribute__((always_inline)) inline void
#endif
fd_selected_file(struct ap_fd_call *call,struct ap_fd_accept *accept,int publish) {
    struct file *file=(struct file *)(call->selection.word&~3ULL);
    call->selected_file=file?fd_file(file):0;
    if(file && !call->selected_file) {
        if(accept)accept->problem|=AP_FD_MISSING;else call->original.problem|=AP_FD_MISSING;
        return;
    }
    if(publish) {
        struct ap_original_selection *selected=&call->original.selection;
        selected->file=call->selected_file;selected->fdput_flags=call->selection.word&1;
        if(__sync_val_compare_and_swap(&selected->ready,0,1)!=0)
            call->original.problem|=AP_FD_DUPLICATE;
    }
}
#endif
