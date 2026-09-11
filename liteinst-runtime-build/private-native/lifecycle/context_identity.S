.intel_syntax noprefix
.text
.global pl_fs_role
.hidden pl_fs_role
.type pl_fs_role,@function
pl_fs_role:
    xor eax, eax
    mov ecx, 2
    cmp rdx, rsi
    cmove rax, rcx
    mov ecx, 1
    cmp rdx, rdi
    cmove rax, rcx
    xor ecx, ecx
    cmp rdi, rsi
    cmove rax, rcx
    test rdi, rdi
    cmovz rax, rcx
    ret
.size pl_fs_role,.-pl_fs_role
.section .note.GNU-stack,"",@progbits
