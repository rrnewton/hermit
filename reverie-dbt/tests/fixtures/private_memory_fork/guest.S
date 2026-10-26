/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the root LICENSE file. */
.text
.global _start
_start:
    mov 16(%rsp), %rax
    cmpb $'h', 1(%rax)
    je shared
    mov $57, %eax
    syscall
    test %rax, %rax
    js failed
    jz done
    mov %rax, %rdi
    lea wait_status(%rip), %rsi
    xor %edx, %edx
    xor %r10d, %r10d
    mov $61, %eax
    syscall
    test %rax, %rax
    js failed
    cmpl $0, wait_status(%rip)
    jne failed
    jmp output
shared:
    mov $56, %eax
    mov $0x111, %edi /* CLONE_VM | SIGCHLD */
    lea child_stack_end(%rip), %rsi
    xor %edx, %edx
    xor %r10d, %r10d
    xor %r8d, %r8d
    syscall
    test %rax, %rax
    js failed
    jz child
    /* Parent waits for an actual child application syscall, not clone return. */
    xor %eax, %eax
    mov $199, %edi
    lea byte(%rip), %rsi
    mov $1, %edx
    syscall
    cmp $1, %rax
    jne failed
    mov $1, %eax
    mov $197, %edi
    lea byte(%rip), %rsi
    mov $1, %edx
    syscall
    cmp $1, %rax
    jne failed
    jmp done
child:
    mov $1, %eax
    mov $200, %edi
    lea byte(%rip), %rsi
    mov $1, %edx
    syscall
    cmp $1, %rax
    jne failed
    /* Only the supervisor releases us, after waitpid proves parent death. */
    xor %eax, %eax
    mov $198, %edi
    lea byte(%rip), %rsi
    mov $1, %edx
    syscall
    cmp $1, %rax
    jne failed
output:
    mov $1, %eax
    mov $1, %edi
    lea message(%rip), %rsi
    mov $message_size, %edx
    syscall
    cmp $message_size, %rax
    jne failed
done:
    xor %edi, %edi
    jmp exit
failed:
    mov $2, %edi
exit:
    mov $60, %eax
    syscall
    ud2
.section .rodata
message: .ascii "private memory fork guest completed\n"
.set message_size, .-message
.section .bss
.align 16
byte: .skip 1
wait_status: .skip 4
.align 16
child_stack: .skip 65536
child_stack_end:
.section .note.GNU-stack,"",@progbits
