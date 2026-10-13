/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * Licensed under the BSD-style license in the LICENSE file.
 */

/* Static, freestanding x86-64 guest. File-scope assembly makes RDTSC the
 * literal ELF entry instruction, before any guest syscall or libc startup.
 * The sole CLONE_THREAD child also executes RDTSC before its first syscall.
 * Successful completion needs both real Detcore thread registrations and the
 * child's consuming exit, without depending on a SIGCHLD callback.
 *
 * child_clear is explicitly initialized to 1. Neither SETTID birth-store flag
 * is used. Only the exit clear turns it into 0; this is not a regression for
 * clone's parent/child TID birth stores or proof that a blocking wake occurred.
 */
#if !defined(__x86_64__)
#error "This first-event guest requires the x86-64 Linux syscall ABI"
#endif

__asm__(
    ".section .text\n"
    ".global _start\n"
    ".type _start,@function\n"
    "_start:\n"
    "  rdtsc\n"
    "  mov $1, %eax\n"
    "  mov $1, %edi\n"
    "  lea root_message(%rip), %rsi\n"
    "  mov $root_message_end-root_message, %edx\n"
    "  syscall\n"
    "  cmp $root_message_end-root_message, %rax\n"
    "  jne failed\n"

    /* CLONE_VM|FS|FILES|SIGHAND|THREAD|SYSVSEM|CHILD_CLEARTID. */
    "  mov $56, %eax\n"
    "  mov $0x250f00, %edi\n"
    "  lea child_stack_end(%rip), %rsi\n"
    "  xor %edx, %edx\n"
    "  lea child_clear(%rip), %r10\n"
    "  xor %r8d, %r8d\n"
    "  syscall\n"
    "  test %rax, %rax\n"
    "  js failed\n"
    "  jz child\n"

    "join_child:\n"
    "  mov child_clear(%rip), %eax\n"
    "  test %eax, %eax\n"
    "  jz joined\n"
    "  cmp $1, %eax\n"
    "  jne failed\n"
    "  mov $202, %eax\n"
    "  lea child_clear(%rip), %rdi\n"
    "  xor %esi, %esi\n" /* FUTEX_WAIT; matches the kernel's exit wake. */
    "  mov $1, %edx\n"
    "  xor %r10d, %r10d\n"
    "  xor %r8d, %r8d\n"
    "  xor %r9d, %r9d\n"
    "  syscall\n"
    "  test %rax, %rax\n"
    "  jz join_child\n"
    "  cmp $-11, %rax\n" /* EAGAIN: clear won before the wait. */
    "  je join_child\n"
    "  cmp $-4, %rax\n" /* EINTR: recheck the exact clear word. */
    "  je join_child\n"
    "  jmp failed\n"

    "joined:\n"
    "  mov $1, %eax\n"
    "  mov $1, %edi\n"
    "  lea done_message(%rip), %rsi\n"
    "  mov $done_message_end-done_message, %edx\n"
    "  syscall\n"
    "  cmp $done_message_end-done_message, %rax\n"
    "  jne failed\n"
    "  mov $231, %eax\n"
    "  xor %edi, %edi\n"
    "  syscall\n"
    "  ud2\n"

    "child:\n"
    "  rdtsc\n"
    "  mov $1, %eax\n"
    "  mov $1, %edi\n"
    "  lea child_message(%rip), %rsi\n"
    "  mov $child_message_end-child_message, %edx\n"
    "  syscall\n"
    "  cmp $child_message_end-child_message, %rax\n"
    "  jne failed\n"
    "  mov $60, %eax\n"
    "  xor %edi, %edi\n"
    "  syscall\n"
    "  ud2\n"

    "failed:\n"
    "  mov $231, %eax\n"
    "  mov $90, %edi\n"
    "  syscall\n"
    "  ud2\n"
    ".size _start, .-_start\n"

    ".section .rodata\n"
    "root_message: .ascii \"first-event root\\n\"\n"
    "root_message_end:\n"
    "child_message: .ascii \"first-event child\\n\"\n"
    "child_message_end:\n"
    "done_message: .ascii \"first-event done\\n\"\n"
    "done_message_end:\n"
    ".section .data\n"
    ".balign 4\n"
    "child_clear: .long 1\n"
    ".section .bss\n"
    ".balign 16\n"
    "child_stack: .skip 65536\n"
    "child_stack_end:\n"
    ".section .note.GNU-stack,\"\",@progbits\n");
