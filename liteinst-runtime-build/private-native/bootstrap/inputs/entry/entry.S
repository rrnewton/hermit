#include "entry.h"

.section .text.pe_kernel_entry,"ax",@progbits
.globl pe_kernel_entry
.hidden pe_kernel_entry
.type pe_kernel_entry,@function
pe_kernel_entry:
    endbr64
    movq %rax, pe_entry_state+PE_RAX(%rip)
    movq %rbx, pe_entry_state+PE_RBX(%rip)
    movq %rcx, pe_entry_state+PE_RCX(%rip)
    movq %rdx, pe_entry_state+PE_RDX(%rip)
    movq %rsi, pe_entry_state+PE_RSI(%rip)
    movq %rdi, pe_entry_state+PE_RDI(%rip)
    movq %rbp, pe_entry_state+PE_RBP(%rip)
    movq %rsp, pe_entry_state+PE_RSP(%rip)
    movq %r8, pe_entry_state+PE_R8(%rip)
    movq %r9, pe_entry_state+PE_R9(%rip)
    movq %r10, pe_entry_state+PE_R10(%rip)
    movq %r11, pe_entry_state+PE_R11(%rip)
    movq %r12, pe_entry_state+PE_R12(%rip)
    movq %r13, pe_entry_state+PE_R13(%rip)
    movq %r14, pe_entry_state+PE_R14(%rip)
    movq %r15, pe_entry_state+PE_R15(%rip)
    leaq pe_scratch_end(%rip), %rsp
    pushfq
    popq pe_entry_state+PE_FLAGS(%rip)
    movw %cs, pe_entry_state+PE_CS(%rip)
    movw %ss, pe_entry_state+PE_SS(%rip)
    movw %ds, pe_entry_state+PE_DS(%rip)
    movw %es, pe_entry_state+PE_ES(%rip)
    movw %fs, pe_entry_state+PE_FS(%rip)
    movw %gs, pe_entry_state+PE_GS(%rip)
    cld
    leaq pe_kernel_entry(%rip), %rax
    movq %rax, pe_entry_state+PE_PC(%rip)
    movq $PE_VERSION, pe_entry_state+PE_VERSION_OFF(%rip)
    movq $PE_RECORD_SIZE, pe_entry_state+PE_SIZE_OFF(%rip)
    movq $PE_VALID_GPRS, pe_entry_state+PE_VALID_OFF(%rip)
    leaq pe_entry_state(%rip), %rax
    movq %rax, pe_entry_state+PE_STORAGE_BEGIN(%rip)
    leaq pe_scratch_end(%rip), %rax
    movq %rax, pe_entry_state+PE_STORAGE_END(%rip)
    movq %rax, pe_entry_state+PE_STACK_END(%rip)
    leaq pe_scratch_begin(%rip), %rax
    movq %rax, pe_entry_state+PE_STACK_BEGIN(%rip)
    movq $PE_FAILURE_STACK, pe_entry_state+PE_FAILURE_OFF(%rip)
    testq $15, pe_entry_state+PE_RSP(%rip)
    jnz .Lterminal
    cmpq $0, pe_entry_state+PE_RSP(%rip)
    je .Lterminal
    movq $PE_FAILURE_CPU, pe_entry_state+PE_FAILURE_OFF(%rip)
    xorl %eax, %eax
    cpuid
    cmpl $13, %eax
    jb .Lterminal
    movl $1, %eax
    cpuid
    andl $0x0c000000, %ecx
    cmpl $0x0c000000, %ecx
    jne .Lterminal
    xorl %ecx, %ecx
    xgetbv
    testl %edx, %edx
    jnz .Lterminal
    movl %eax, %r12d
    andl $~PE_XSTATE_MASK, %eax
    jnz .Lterminal
    movl %r12d, %eax
    andl $3, %eax
    cmpl $3, %eax
    jne .Lterminal
    movq %r12, pe_entry_state+PE_XCR0(%rip)
    movl $13, %eax
    xorl %ecx, %ecx
    cpuid
    movl %eax, %r13d
    movl %edx, %r14d
    shlq $32, %r14
    orq %r14, %r13
    movq %r13, pe_entry_state+PE_CPU_MASK(%rip)
    andl %r12d, %eax
    cmpl %r12d, %eax
    jne .Lterminal
    movq $PE_FAILURE_XSTATE, pe_entry_state+PE_FAILURE_OFF(%rip)
    cmpl $576, %ebx
    jb .Lterminal
    cmpl $PE_XSTATE_CAPACITY, %ebx
    ja .Lterminal
    movq %rbx, pe_entry_state+PE_XSTATE_SIZE(%rip)
    movl %r12d, %eax
    xorl %edx, %edx
    xsave64 pe_entry_state+PE_XSTATE(%rip)
    orq $PE_VALID_XSTATE, pe_entry_state+PE_VALID_OFF(%rip)
    movq $PE_FAILURE_FS, pe_entry_state+PE_FAILURE_OFF(%rip)
    movl $158, %eax
    movl $0x1003, %edi
    leaq pe_entry_state+PE_FS_BASE(%rip), %rsi
    syscall
    testq %rax, %rax
    jnz .Lsyscall_failure
    orq $PE_VALID_FS, pe_entry_state+PE_VALID_OFF(%rip)
    movq $PE_FAILURE_GS, pe_entry_state+PE_FAILURE_OFF(%rip)
    movl $158, %eax
    movl $0x1004, %edi
    leaq pe_entry_state+PE_GS_BASE(%rip), %rsi
    syscall
    testq %rax, %rax
    jnz .Lsyscall_failure
    orq $PE_VALID_GS, pe_entry_state+PE_VALID_OFF(%rip)
    movq $PE_FAILURE_SHSTK, pe_entry_state+PE_FAILURE_OFF(%rip)
    movl $158, %eax
    movl $0x5005, %edi
    leaq pe_entry_state+PE_SHSTK(%rip), %rsi
    syscall
    movq %rax, pe_entry_state+PE_SHSTK_RESULT(%rip)
    cmpq $-22, %rax
    je .Lready
    testq %rax, %rax
    jnz .Lsyscall_failure
    cmpq $0, pe_entry_state+PE_SHSTK(%rip)
    jne .Lterminal
.Lready:
    orq $PE_VALID_POLICY, pe_entry_state+PE_VALID_OFF(%rip)
    movq $0, pe_entry_state+PE_FAILURE_OFF(%rip)
    leaq pe_entry_state(%rip), %rdi
    call pe_continue
    movq $PE_FAILURE_RETURN, pe_entry_state+PE_FAILURE_OFF(%rip)
    jmp .Lterminal
.Lsyscall_failure:
    movq %rax, pe_entry_state+PE_ERROR_OFF(%rip)
.Lterminal:
    movl $231, %eax
    movl $127, %edi
    syscall
    ud2
.size pe_kernel_entry, .-pe_kernel_entry
.hidden pe_continue

.section .bss.pe_storage,"aw",@nobits
.balign 4096
.globl pe_entry_state
.hidden pe_entry_state
.type pe_entry_state,@object
pe_entry_state:
    .skip PE_RECORD_SIZE
.size pe_entry_state, .-pe_entry_state
.balign 4096
.globl pe_scratch_begin
.hidden pe_scratch_begin
pe_scratch_begin:
    .skip PE_STACK_SIZE
.globl pe_scratch_end
.hidden pe_scratch_end
pe_scratch_end:

.section .note.GNU-stack,"",@progbits
