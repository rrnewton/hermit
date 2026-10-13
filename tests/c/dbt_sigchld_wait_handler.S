/* Actual x86-64 handler-entry RSP, with no SA_SIGINFO assumption. */
.text
.globl wait_handler_entry
.type wait_handler_entry,@function
wait_handler_entry:
    mov %rsp, %rsi
    jmp wait_handler_inspect
.size wait_handler_entry, .-wait_handler_entry

/* SysV arguments: target, status, rusage. All six syscall registers are
 * intentional, including Linux-ignored r8/r9 canaries. */
.globl inspected_wait4
.type inspected_wait4,@function
inspected_wait4:
    mov %rdx, %r10
    xor %edx, %edx
    mov $0x13579bdf, %r8d
    mov $0x2468ace0, %r9d
    mov %rsp, wait_original_sp(%rip)
    mov $61, %eax
.globl inspected_wait4_instruction
inspected_wait4_instruction:
    syscall
.globl inspected_wait4_fallthrough
inspected_wait4_fallthrough:
    incl wait_returned(%rip)
    ret
.size inspected_wait4, .-inspected_wait4
.section .note.GNU-stack,"",@progbits
