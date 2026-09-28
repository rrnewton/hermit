.intel_syntax noprefix
.text
.hidden pl_tls
.hidden pl_fs_role
.hidden reverie_preload_trusted_syscall_ip

.macro pl_require condition
    lea r10, [rip + .Ltls_fatal]
    lea r11, [rip + 991f]
    cmov\condition r10, r11
    jmp r10
991:
.endm

.macro pl_owner
    lea rbx, [rip + pl_tls]
    cmp qword ptr [rbx + 32], 2
    pl_require e
    mov eax, 186
    call reverie_preload_trusted_syscall_ip
    cmp rax, [rbx + 8]
    pl_require e
.endm

.macro pl_set_fs
    mov eax, 158
    mov edi, 0x1002
    call reverie_preload_trusted_syscall_ip
    test rax, rax
    pl_require z
.endm

.global pl_context_enter
.macro pl_set_gs
    mov eax, 158
    mov edi, 0x1001
    call reverie_preload_trusted_syscall_ip
    test rax, rax
    pl_require z
.endm

.macro pl_selectors
    mov ax, fs
    test ax, ax
    pl_require z
    mov ax, gs
    test ax, ax
    pl_require z
.endm

.macro pl_private
    pl_selectors
    mov eax, 158
    mov edi, 0x1003
    mov rsi, rsp
    call reverie_preload_trusted_syscall_ip
    test rax, rax
    pl_require z
    mov rax, [rsp]
    cmp rax, [rbx]
    pl_require e
    mov eax, 158
    mov edi, 0x1004
    mov rsi, rsp
    call reverie_preload_trusted_syscall_ip
    test rax, rax
    pl_require z
    mov rax, [rsp]
    cmp rax, [rbx + 40]
    pl_require e
.endm

.hidden pl_context_enter
.type pl_context_enter,@function
pl_context_enter:
    push rbx
    push r12
    sub rsp, 24
    pl_owner
    pl_selectors
    mov eax, 158
    mov edi, 0x1003
    mov rsi, rsp
    call reverie_preload_trusted_syscall_ip
    test rax, rax
    pl_require z
    mov r12, [rsp]
    mov rdi, [rbx]
    mov rsi, [rbx + 16]
    mov rdx, r12
    call pl_fs_role
    test rax, rax
    pl_require nz
    mov r12, rax
    mov eax, 158
    mov edi, 0x1004
    mov rsi, rsp
    call reverie_preload_trusted_syscall_ip
    test rax, rax
    pl_require z
    mov rdx, [rbx + 40]
    mov rcx, [rbx + 48]
    cmp r12, 2
    cmove rdx, rcx
    cmp rdx, [rsp]
    pl_require e
    mov rsi, [rbx + 40]
    pl_set_gs
    mov rsi, [rbx]
    pl_set_fs
    mov rax, r12
    add rsp, 24
    pop r12
    pop rbx
    ret
.size pl_context_enter,.-pl_context_enter

.global pl_context_leave
.hidden pl_context_leave
.type pl_context_leave,@function
pl_context_leave:
    push rbx
    push r12
    push r13
    sub rsp, 16
    mov r12, rdi
    mov r13, rsi
    pl_owner
    pl_private
    cmp r13, 2
    pl_require be
    cmp r12, 1
    pl_require ae
    cmp r12, 2
    pl_require be
    lea r10, [rip + .Ltls_restore]
    lea r11, [rip + .Ltls_deferred]
    cmp r13, 2
    cmove r10, r11
    jmp r10
.Ltls_deferred:
    cmp r12, 2
    pl_require e
    xor eax, eax
    mov edx, 1
    lock cmpxchg [rbx + 24], rdx
    test rax, rax
    pl_require z
    jmp .Ltls_leave_done
.Ltls_restore:
    mov rsi, [rbx + 40]
    mov rdx, [rbx + 48]
    cmp r12, 2
    cmove rsi, rdx
    pl_set_gs
    mov rsi, [rbx]
    mov rdx, [rbx + 16]
    cmp r12, 2
    cmove rsi, rdx
    pl_set_fs
.Ltls_leave_done:
    add rsp, 16
    pop r13
    pop r12
    pop rbx
    ret
.size pl_context_leave,.-pl_context_leave

.global pl_context_finish_deferred
.hidden pl_context_finish_deferred
.type pl_context_finish_deferred,@function
pl_context_finish_deferred:
    push rbx
    sub rsp, 16
    pl_owner
    pl_private
    mov eax, 1
    xor edx, edx
    lock cmpxchg [rbx + 24], rdx
    cmp rax, 1
    pl_require e
    mov rsi, [rbx + 48]
    pl_set_gs
    mov rsi, [rbx + 16]
    pl_set_fs
    add rsp, 16
    pop rbx
    ret
.size pl_context_finish_deferred,.-pl_context_finish_deferred

.global pl_guest_arch_prctl
.hidden pl_guest_arch_prctl
.type pl_guest_arch_prctl,@function
pl_guest_arch_prctl:
    push rbx
    push r12
    push r13
    push r14
    push r15
    sub rsp, 16
    mov r12, rdi
    mov r13, rsi
    mov r14, rdx
    pl_owner
    pl_private
    cmp qword ptr [rbx + 24], 1
    pl_require e
    cmp r12, 0x1001
    pl_require ae
    cmp r12, 0x1004
    pl_require be
    mov rsi, [rbx + 48]
    pl_set_gs
    mov rsi, [rbx + 16]
    pl_set_fs
    mov eax, 158
    mov rdi, r12
    mov rsi, r13
    call reverie_preload_trusted_syscall_ip
    mov [r14], rax
    mov eax, 158
    mov edi, 0x1003
    lea rsi, [r14 + 8]
    call reverie_preload_trusted_syscall_ip
    test rax, rax
    pl_require z
    mov eax, 158
    mov edi, 0x1004
    lea rsi, [r14 + 16]
    call reverie_preload_trusted_syscall_ip
    test rax, rax
    pl_require z
    pl_selectors
    mov rsi, [rbx + 40]
    pl_set_gs
    mov rsi, [rbx]
    pl_set_fs
    pl_private
    add rsp, 16
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    ret
.size pl_guest_arch_prctl,.-pl_guest_arch_prctl

.Ltls_fatal:
    mov eax, 231
    mov edi, 127
    call reverie_preload_trusted_syscall_ip
    ud2
.section .note.GNU-stack,"",@progbits
.section .text.pl_explicit_context,"ax",@progbits
.macro pl_explicit_owner
    mov rbx, rdi
    test rbx, rbx
    pl_require nz
    test rbx, 7
    pl_require z
    cmp qword ptr [rbx + 32], 2
    pl_require e
    mov eax, 186
    call reverie_preload_trusted_syscall_ip
    cmp rax, [rbx + 8]
    pl_require e
.endm
.global pl_context_enter_control
.hidden pl_context_enter_control
.type pl_context_enter_control,@function
pl_context_enter_control:
    push rbx
    push r12
    sub rsp, 24
    pl_explicit_owner
    pl_selectors
    mov eax, 158
    mov edi, 0x1003
    mov rsi, rsp
    call reverie_preload_trusted_syscall_ip
    test rax, rax
    pl_require z
    mov r12, [rsp]
    mov rdi, [rbx]
    mov rsi, [rbx + 16]
    mov rdx, r12
    call pl_fs_role
    test rax, rax
    pl_require nz
    mov r12, rax
    mov eax, 158
    mov edi, 0x1004
    mov rsi, rsp
    call reverie_preload_trusted_syscall_ip
    test rax, rax
    pl_require z
    mov rdx, [rbx + 40]
    mov rcx, [rbx + 48]
    cmp r12, 2
    cmove rdx, rcx
    cmp rdx, [rsp]
    pl_require e
    mov rsi, [rbx + 40]
    pl_set_gs
    mov rsi, [rbx]
    pl_set_fs
    mov rax, r12
    add rsp, 24
    pop r12
    pop rbx
    ret
.size pl_context_enter_control,.-pl_context_enter_control
.global pl_context_leave_control
.hidden pl_context_leave_control
.type pl_context_leave_control,@function
pl_context_leave_control:
    push rbx
    push r12
    push r13
    sub rsp, 16
    mov r12, rsi
    mov r13, rdx
    pl_explicit_owner
    pl_private
    cmp r13, 2
    pl_require be
    cmp r12, 1
    pl_require ae
    cmp r12, 2
    pl_require be
    lea r10, [rip + .Lexplicit_tls_restore]
    lea r11, [rip + .Lexplicit_tls_deferred]
    cmp r13, 2
    cmove r10, r11
    jmp r10
.Lexplicit_tls_deferred:
    cmp r12, 2
    pl_require e
    xor eax, eax
    mov edx, 1
    lock cmpxchg [rbx + 24], rdx
    test rax, rax
    pl_require z
    jmp .Lexplicit_tls_leave_done
.Lexplicit_tls_restore:
    mov rsi, [rbx + 40]
    mov rdx, [rbx + 48]
    cmp r12, 2
    cmove rsi, rdx
    pl_set_gs
    mov rsi, [rbx]
    mov rdx, [rbx + 16]
    cmp r12, 2
    cmove rsi, rdx
    pl_set_fs
.Lexplicit_tls_leave_done:
    add rsp, 16
    pop r13
    pop r12
    pop rbx
    ret
.size pl_context_leave_control,.-pl_context_leave_control
.global pl_context_finish_deferred_control
.hidden pl_context_finish_deferred_control
.type pl_context_finish_deferred_control,@function
pl_context_finish_deferred_control:
    push rbx
    sub rsp, 16
    pl_explicit_owner
    pl_private
    mov eax, 1
    xor edx, edx
    lock cmpxchg [rbx + 24], rdx
    cmp rax, 1
    pl_require e
    mov rsi, [rbx + 48]
    pl_set_gs
    mov rsi, [rbx + 16]
    pl_set_fs
    add rsp, 16
    pop rbx
    ret
.size pl_context_finish_deferred_control,.-pl_context_finish_deferred_control
