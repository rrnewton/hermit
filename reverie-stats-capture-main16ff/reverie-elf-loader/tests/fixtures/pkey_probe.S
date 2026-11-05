/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Freestanding interpreter: sample live PKRU, then attempt a user data read
 * from the main's AT_ENTRY or this interpreter's own entry. A real signal
 * handler records the protection fault and resumes only the deliberate read.
 * No syscall or procfs read stands in for the tracee's data access. */
.bss
.balign 16
report:
    .space 64
action:
    .space 32
private_stack:
    .space 16384
private_stack_end:

.text
.globl _start
.type _start, @function
_start:
    movq %rsp, %r12
    leaq private_stack_end(%rip), %rsp
    xorl %ecx, %ecx
    rdpkru
    movl %eax, report+8(%rip)
#ifdef PKEY_PROBE_INTERPRETER
    leaq _start(%rip), %r13
#else
    movq (%r12), %rcx
    leaq 16(%r12,%rcx,8), %rsi
.Lenv:
    cmpq $0, (%rsi)
    leaq 8(%rsi), %rsi
    jne .Lenv
.Laux:
    movq (%rsi), %rax
    cmpq $9, %rax                    /* AT_ENTRY */
    je .Lentry
    testq %rax, %rax
    je .Lbad_probe
    addq $16, %rsi
    jmp .Laux
.Lentry:
    movq 8(%rsi), %r13
#endif
    movq %r13, report+16(%rip)
    leaq .Lsegv(%rip), %rax
    movq %rax, action(%rip)
    movq $0x04000004, action+8(%rip)  /* SA_RESTORER | SA_SIGINFO */
    leaq .Lsigreturn(%rip), %rax
    movq %rax, action+16(%rip)
    movl $13, %eax                   /* rt_sigaction */
    movl $11, %edi                   /* SIGSEGV */
    leaq action(%rip), %rsi
    xorl %edx, %edx
    movl $8, %r10d                   /* kernel sigset size */
    syscall
    testq %rax, %rax
    jne .Lbad_probe
.Lread:
    movb (%r13), %al
    movb %al, report+32(%rip)
    movq $1, report+24(%rip)          /* readable */
.Lresume:
    xorl %ecx, %ecx
    rdpkru
    movl %eax, report+12(%rip)        /* after read or rt_sigreturn */
    movabsq $0x3130554b50464c45, %rax /* "ELFPKU01" */
    movq %rax, report(%rip)
    leaq report(%rip), %r12
    movl $64, %r13d
.Lwrite:
    movl $1, %eax
    movl $1, %edi
    movq %r12, %rsi
    movq %r13, %rdx
    syscall
    testq %rax, %rax
    jle .Lbad_probe
    addq %rax, %r12
    subq %rax, %r13
    jnz .Lwrite
    xorl %edi, %edi
    jmp .Lexit
.Lbad_probe:
    movl $95, %edi
.Lexit:
    movl $231, %eax
    syscall
    ud2
.size _start, .-_start

/* x86-64 Linux siginfo and ucontext ABI. The handler never changes the saved
 * FP/xstate image. rt_sigreturn restores the faulting task's original PKRU. */
.Lsegv:
    cmpl $11, %edi
    jne .Lbad_probe
    cmpl $4, 8(%rsi)                 /* SEGV_PKUERR */
    jne .Lbad_probe
    cmpl $1, 32(%rsi)                /* si_pkey */
    jne .Lbad_probe
    movq 16(%rsi), %rax              /* si_addr */
    cmpq report+16(%rip), %rax
    jne .Lbad_probe
    leaq .Lread(%rip), %rcx
    cmpq %rcx, 168(%rdx)             /* uc_mcontext.rip */
    jne .Lbad_probe
    movl %edi, report+40(%rip)
    movl 8(%rsi), %ecx
    movl %ecx, report+44(%rip)
    movl 32(%rsi), %ecx
    movl %ecx, report+48(%rip)
    movq %rax, report+56(%rip)
    leaq .Lresume(%rip), %rax
    movq %rax, 168(%rdx)
    ret
.Lsigreturn:
    movl $15, %eax                   /* rt_sigreturn */
    syscall
    ud2

.section .note.GNU-stack,"",@progbits
