/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the root LICENSE file. */
.text
.global _start
_start:
    xor %eax, %eax
    mov $198, %edi
    lea byte(%rip), %rsi
    mov $1, %edx
    syscall
    cmp $1, %rax
    jne failed
    xor %edi, %edi
    jmp done
failed:
    mov $2, %edi
done:
    mov $60, %eax
    syscall
    ud2
.bss
byte:
    .skip 1
.section .note.GNU-stack,"",@progbits
