#include "child.h"
.section .text.pl_child_test_capture_ok,"ax",@progbits
.globl pl_child_test_capture_ok
.hidden pl_child_test_capture_ok
.type pl_child_test_capture_ok,@function
pl_child_test_capture_ok:
    endbr64
    xorl %eax, %eax
    cmpq $PE_VERSION, PE_VERSION_OFF(%rdi)
    jne .Lcapture_done
    cmpq $PE_RECORD_SIZE, PE_SIZE_OFF(%rdi)
    jne .Lcapture_done
    cmpq $PE_VALID_ALL, PE_VALID_OFF(%rdi)
    jne .Lcapture_done
    cmpq $0, PE_FAILURE_OFF(%rdi)
    jne .Lcapture_done
    cmpq $0, PE_RAX(%rdi)
    jne .Lcapture_done
    cmpq $PL_CHILD_CLONE_FLAGS, PE_RDI(%rdi)
    jne .Lcapture_done
    cmpq %rdi, PE_R12(%rdi)
    jne .Lcapture_done
    cmpq $0, PE_R8(%rdi)
    jne .Lcapture_done
    movq PL_CHILD_BOOT_HIGH(%rdi), %rdx
    subq $16, %rdx
    cmpq %rdx, PE_RSI(%rdi)
    jne .Lcapture_done
    addq $8, %rdx
    cmpq %rdx, PE_RSP(%rdi)
    jne .Lcapture_done
    leaq PL_CHILD_PARENT_TID(%rdi), %rdx
    cmpq %rdx, PE_RDX(%rdi)
    jne .Lcapture_done
    leaq PL_CHILD_KERNEL_TID(%rdi), %rdx
    cmpq %rdx, PE_R10(%rdi)
    jne .Lcapture_done
    leaq reverie_preload_trusted_syscall_return_ip(%rip), %rdx
    cmpq %rdx, PE_RCX(%rdi)
    jne .Lcapture_done
    leaq pl_child_entry(%rip), %rdx
    cmpq %rdx, PE_PC(%rdi)
    jne .Lcapture_done
    movq PE_R11(%rdi), %rdx
    cmpq %rdx, PE_FLAGS(%rdi)
    jne .Lcapture_done
    movl $1, %eax
.Lcapture_done:
    ret
.size pl_child_test_capture_ok, .-pl_child_test_capture_ok
.hidden reverie_preload_trusted_syscall_return_ip
.section .note.GNU-stack,"",@progbits
