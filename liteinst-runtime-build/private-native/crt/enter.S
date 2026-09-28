.text
.globl pm_enter_private_crt
.hidden pm_enter_private_crt
.type pm_enter_private_crt,@function
pm_enter_private_crt:
  movq %rdi, %r12
  movq %rsi, %rax
  movq %rdx, %rsp
  xorl %edx, %edx
  cld
  jmp *%rax
.size pm_enter_private_crt, .-pm_enter_private_crt
.section .note.GNU-stack,"",@progbits
