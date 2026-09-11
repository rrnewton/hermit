#include "child.h"

.section .text.pl_child_kernel,"ax",@progbits
.globl pl_child_kernel
.hidden pl_child_kernel
.type pl_child_kernel,@function
pl_child_kernel:
    endbr64
    pushq %r12
    movq %rdi, %r12
    movl $56, %eax
    movl $PL_CHILD_CLONE_FLAGS, %edi
    movq PL_CHILD_BOOT_HIGH(%r12), %rsi
    subq $16, %rsi
    leaq PL_CHILD_PARENT_TID(%r12), %rdx
    leaq PL_CHILD_KERNEL_TID(%r12), %r10
    xorl %r8d, %r8d
    call reverie_preload_trusted_syscall_ip
    popq %r12
    ret
.size pl_child_kernel, .-pl_child_kernel

.section .text.pl_child_entry,"ax",@progbits
.globl pl_child_entry
.hidden pl_child_entry
.type pl_child_entry,@function
pl_child_entry:
    endbr64
    pushq %r15
    movq 8(%rsp), %r15
    movq %rax, PE_RAX(%r15)
    movq %rbx, PE_RBX(%r15)
    movq %rcx, PE_RCX(%r15)
    movq %rdx, PE_RDX(%r15)
    movq %rsi, PE_RSI(%r15)
    movq %rdi, PE_RDI(%r15)
    movq %rbp, PE_RBP(%r15)
    movq %r8, PE_R8(%r15)
    movq %r9, PE_R9(%r15)
    movq %r10, PE_R10(%r15)
    movq %r11, PE_R11(%r15)
    movq %r12, PE_R12(%r15)
    movq %r13, PE_R13(%r15)
    movq %r14, PE_R14(%r15)
    movq (%rsp), %rax
    movq %rax, PE_R15(%r15)
    leaq 8(%rsp), %rax
    movq %rax, PE_RSP(%r15)
    pushfq
    popq PE_FLAGS(%r15)
    movw %cs, PE_CS(%r15)
    movw %ss, PE_SS(%r15)
    movw %ds, PE_DS(%r15)
    movw %es, PE_ES(%r15)
    movw %fs, PE_FS(%r15)
    movw %gs, PE_GS(%r15)
    cld
    leaq pl_child_entry(%rip), %rax
    movq %rax, PE_PC(%r15)
    movq $PE_VERSION, PE_VERSION_OFF(%r15)
    movq $PE_RECORD_SIZE, PE_SIZE_OFF(%r15)
    movq $PE_VALID_GPRS, PE_VALID_OFF(%r15)
    movq %r15, PE_STORAGE_BEGIN(%r15)
    leaq PE_RECORD_SIZE(%r15), %rax
    movq %rax, PE_STORAGE_END(%r15)
    movq PL_CHILD_BOOT_LOW(%r15), %rax
    movq %rax, PE_STACK_BEGIN(%r15)
    movq PL_CHILD_BOOT_HIGH(%r15), %rax
    movq %rax, PE_STACK_END(%r15)
    movq $PE_FAILURE_CPU, PE_FAILURE_OFF(%r15)
    xorl %eax, %eax
    cpuid
    cmpl $13, %eax
    jb .Lchild_capture_failure
    movl $1, %eax
    cpuid
    andl $0x0c000000, %ecx
    cmpl $0x0c000000, %ecx
    jne .Lchild_capture_failure
    xorl %ecx, %ecx
    xgetbv
    testl %edx, %edx
    jnz .Lchild_capture_failure
    movl %eax, %r12d
    andl $~PE_XSTATE_MASK, %eax
    jnz .Lchild_capture_failure
    movl %r12d, %eax
    andl $3, %eax
    cmpl $3, %eax
    jne .Lchild_capture_failure
    movq %r12, PE_XCR0(%r15)
    movl $13, %eax
    xorl %ecx, %ecx
    cpuid
    movl %eax, %r13d
    movl %edx, %r14d
    shlq $32, %r14
    orq %r14, %r13
    movq %r13, PE_CPU_MASK(%r15)
    andl %r12d, %eax
    cmpl %r12d, %eax
    jne .Lchild_capture_failure
    movq $PE_FAILURE_XSTATE, PE_FAILURE_OFF(%r15)
    cmpl $576, %ebx
    jb .Lchild_capture_failure
    cmpl $PE_XSTATE_CAPACITY, %ebx
    ja .Lchild_capture_failure
    movq %rbx, PE_XSTATE_SIZE(%r15)
    movl %r12d, %eax
    xorl %edx, %edx
    xsave64 PE_XSTATE(%r15)
    orq $PE_VALID_XSTATE, PE_VALID_OFF(%r15)
    movq $PE_FAILURE_FS, PE_FAILURE_OFF(%r15)
    movl $158, %eax
    movl $0x1003, %edi
    leaq PE_FS_BASE(%r15), %rsi
    call reverie_preload_trusted_syscall_ip
    testq %rax, %rax
    jnz .Lchild_syscall_failure
    orq $PE_VALID_FS, PE_VALID_OFF(%r15)
    movq $PE_FAILURE_GS, PE_FAILURE_OFF(%r15)
    movl $158, %eax
    movl $0x1004, %edi
    leaq PE_GS_BASE(%r15), %rsi
    call reverie_preload_trusted_syscall_ip
    testq %rax, %rax
    jnz .Lchild_syscall_failure
    orq $PE_VALID_GS, PE_VALID_OFF(%r15)
    movq $PE_FAILURE_SHSTK, PE_FAILURE_OFF(%r15)
    movl $158, %eax
    movl $0x5005, %edi
    leaq PE_SHSTK(%r15), %rsi
    call reverie_preload_trusted_syscall_ip
    movq %rax, PE_SHSTK_RESULT(%r15)
#if defined PL_NATIVE_CET_ABI_UNAVAILABLE && PL_NATIVE_CET_ABI_UNAVAILABLE == 1
    cmpq $-22, %rax
    je .Lchild_cet_ready
#endif
    testq %rax, %rax
    jnz .Lchild_syscall_failure
    cmpq $0, PE_SHSTK(%r15)
    jne .Lchild_capture_failure
.Lchild_cet_ready:
    orq $PE_VALID_POLICY, PE_VALID_OFF(%r15)
    movq $0, PE_FAILURE_OFF(%r15)
    movl $158, %eax
    movl $0x1002, %edi
    movq PL_CHILD_PRIVATE_FS(%r15), %rsi
    call reverie_preload_trusted_syscall_ip
    testq %rax, %rax
    jnz .Lchild_syscall_failure
    movq PL_CHILD_PRIVATE_TOP(%r15), %rsp
    movq %r15, %rdi
    call pl_child_arrived
    ud2
.Lchild_syscall_failure:
    movq %rax, PE_ERROR_OFF(%r15)
.Lchild_capture_failure:
    movq %r15, %rdi
    movq $-95, %rsi
    jmp pl_child_abandon
.size pl_child_entry, .-pl_child_entry

.section .text.pl_child_terminal,"ax",@progbits
.globl pl_child_terminal
.hidden pl_child_terminal
.type pl_child_terminal,@function
pl_child_terminal:
    endbr64
    movq %rdi, %r15
    movq PL_CHILD_TERMINAL_TOP(%r15), %rsp
    movl $158, %eax
    movl $0x1002, %edi
    movq PL_CHILD_EXIT_FS(%r15), %rsi
    call reverie_preload_trusted_syscall_ip
    testq %rax, %rax
    jnz .Lchild_terminal_failure
    movl $158, %eax
    movl $0x1001, %edi
    movq PL_CHILD_EXIT_GS(%r15), %rsi
    call reverie_preload_trusted_syscall_ip
    testq %rax, %rax
    jnz .Lchild_terminal_failure
.Lchild_exit_wait:
    cmpl $1, PL_CHILD_EXIT_RELEASE(%r15)
    je .Lchild_exit
    movl $202, %eax
    leaq PL_CHILD_EXIT_RELEASE(%r15), %rdi
    movl $128, %esi
    xorl %edx, %edx
    xorl %r10d, %r10d
    call reverie_preload_trusted_syscall_ip
    testq %rax, %rax
    jz .Lchild_exit_wait
    cmpq $-11, %rax
    je .Lchild_exit_wait
    cmpq $-4, %rax
    je .Lchild_exit_wait
    jmp .Lchild_terminal_failure
.Lchild_exit:
    movl $6, PL_CHILD_STAGE(%r15)
    movl $60, %eax
    xorl %edi, %edi
    call reverie_preload_trusted_syscall_ip
    ud2
.Lchild_terminal_failure:
    movq %r15, %rdi
    movq %rax, %rsi
    jmp pl_child_abandon
.size pl_child_terminal, .-pl_child_terminal

.section .text.pl_child_abandon,"ax",@progbits
.globl pl_child_abandon
.hidden pl_child_abandon
.type pl_child_abandon,@function
pl_child_abandon:
    endbr64
    movq %rdi, %r15
    movq %rsi, PE_ERROR_OFF(%r15)
    movl $PL_CHILD_FAILED, PL_CHILD_STAGE(%r15)
    lock addl $1, PL_CHILD_EVENT(%r15)
    movl $202, %eax
    leaq PL_CHILD_EVENT(%r15), %rdi
    movl $129, %esi
    movl $0x7fffffff, %edx
    call reverie_preload_trusted_syscall_ip
    movl $60, %eax
    movl $121, %edi
    call reverie_preload_trusted_syscall_ip
    ud2
.size pl_child_abandon, .-pl_child_abandon
.hidden reverie_preload_trusted_syscall_ip
.section .note.GNU-stack,"",@progbits
