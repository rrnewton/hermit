#include "entry.h"
.intel_syntax noprefix
.text
.hidden pl_initial_capture
.hidden reverie_preload_trusted_syscall_ip
.hidden reverie_liteinst_clock_leave
.global pl_restore_and_fault
.hidden pl_restore_and_fault
.type pl_restore_and_fault,@function
pl_restore_and_fault:
    sub rsp, 8
    mov edi, 1
    xor esi, esi
    xor edx, edx
    call reverie_liteinst_clock_leave
    mov eax, 158
    mov edi, 0x1002
    mov rsi, [rip + pl_initial_capture + PE_FS_BASE]
    call reverie_preload_trusted_syscall_ip
    lea r10, [rip + .Linitial_failed]
    lea r11, [rip + .Linitial_fs]
    test rax, rax
    cmovz r10, r11
    jmp r10
.Linitial_fs:
    mov eax, 158
    mov edi, 0x1001
    mov rsi, [rip + pl_initial_capture + PE_GS_BASE]
    call reverie_preload_trusted_syscall_ip
    lea r10, [rip + .Linitial_failed]
    lea r11, [rip + .Linitial_state]
    test rax, rax
    cmovz r10, r11
    jmp r10
.Linitial_state:
    mov ax, word ptr [rip + pl_initial_capture + PE_DS]
    mov ds, ax
    mov ax, word ptr [rip + pl_initial_capture + PE_ES]
    mov es, ax
    mov eax, dword ptr [rip + pl_initial_capture + PE_XCR0]
    mov edx, dword ptr [rip + pl_initial_capture + PE_XCR0 + 4]
    xrstor64 [rip + pl_initial_capture + PE_XSTATE]
    push qword ptr [rip + pl_initial_capture + PE_FLAGS]
    popfq
    mov rax, [rip + pl_initial_capture + PE_RAX]
    mov rbx, [rip + pl_initial_capture + PE_RBX]
    mov rcx, [rip + pl_initial_capture + PE_RCX]
    mov rdx, [rip + pl_initial_capture + PE_RDX]
    mov rsi, [rip + pl_initial_capture + PE_RSI]
    mov rdi, [rip + pl_initial_capture + PE_RDI]
    mov rbp, [rip + pl_initial_capture + PE_RBP]
    mov r8, [rip + pl_initial_capture + PE_R8]
    mov r9, [rip + pl_initial_capture + PE_R9]
    mov r10, [rip + pl_initial_capture + PE_R10]
    mov r11, [rip + pl_initial_capture + PE_R11]
    mov r12, [rip + pl_initial_capture + PE_R12]
    mov r13, [rip + pl_initial_capture + PE_R13]
    mov r14, [rip + pl_initial_capture + PE_R14]
    mov r15, [rip + pl_initial_capture + PE_R15]
    mov rsp, [rip + pl_initial_capture + PE_RSP]
.global pl_initial_fault
.hidden pl_initial_fault
pl_initial_fault:
    cpuid
    ud2
.Linitial_failed:
    mov eax, 231
    mov edi, 127
    call reverie_preload_trusted_syscall_ip
    ud2
.size pl_restore_and_fault,.-pl_restore_and_fault
.section .note.GNU-stack,"",@progbits
