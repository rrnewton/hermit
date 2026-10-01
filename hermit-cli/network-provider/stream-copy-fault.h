/* SPDX-License-Identifier: MIT */
#ifndef HERMIT_STREAM_COPY_FAULT_H
#define HERMIT_STREAM_COPY_FAULT_H
#if defined(__BPF__) && (defined(AP_STREAM_FAULT_MUTATE_DROP_DATA) || \
    defined(AP_STREAM_FAULT_MUTATE_DROP_PRIOR) || defined(AP_STREAM_FAULT_MUTATE_CX) || \
    defined(AP_STREAM_FAULT_MUTATE_SOURCE) || defined(AP_STREAM_FAULT_MUTATE_WINDOW) || \
    defined(AP_STREAM_FAULT_MUTATE_FRONTIER))
#error "failed-copy mutants are host controls, never loadable provider artifacts"
#endif
#define AP_STREAM_FAULT_FUNCTION_IMAGE 0xffffffff821edc90ULL
#define AP_STREAM_FAULT_COPY_IMAGE 0xffffffff81fb85d0ULL
#define AP_STREAM_FAULT_INSN_IMAGE 0xffffffff81fb8cc8ULL
#define AP_STREAM_FAULT_NEXT_IMAGE 0xffffffff81fb8ccdULL
#define AP_STREAM_FAULT_EXTABLE_IMAGE 0xffffffff82d595c0ULL
struct ap_stream_fault_regs {
    u64 r15,r14,r13,r12,bp,bx,r11,r10,r9,r8,ax,cx,dx,si,di,orig_ax,ip,cs,flags,sp,ss;
};
struct ap_stream_fault_state {
    u64 phase,provider,command,call,task,start,file,pointer,frame,attempt;
    u64 iterator,skb,source,requested,before,entry_offset,ubuf,iter_type;
    u64 fault_frame,trap,error,address,observed,nonlinear;
    struct ap_stream_fault_regs regs;
};
#define AP_STREAM_FAULT_BTF_ID 108812U
#endif
