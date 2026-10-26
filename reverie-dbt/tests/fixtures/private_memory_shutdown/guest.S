/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the root LICENSE file. */
.text
.global _start
_start:
    mov $1, %eax
    mov $1, %edi
    lea message(%rip), %rsi
    mov $message_size, %edx
    syscall
    cmp $message_size, %rax
    jne failed
    xor %edi, %edi
    jmp done
failed:
    mov $2, %edi
done:
    mov $60, %eax
    syscall
    ud2
.section .rodata
message:
    .ascii "private memory shutdown guest completed\n"
.set message_size, .-message
.section .note.GNU-stack,"",@progbits
