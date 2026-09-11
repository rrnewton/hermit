.section .text.pe_private_crt_entry,"ax",@progbits
.globl pe_private_crt_entry
.hidden pe_private_crt_entry
.type pe_private_crt_entry,@function
.hidden pe_gnu_crt_start
pe_private_crt_entry:
  testq %rdx, %rdx
  jne .Lbad_launch
  testq $15, %rsp
  jne .Lbad_launch
  testq %r12, %r12
  je .Lbad_launch
  testq $7, %r12
  jne .Lbad_launch
  movl $1, %eax
  xchgl %eax, pe_private_entry_used(%rip)
  testl %eax, %eax
  jne .Lreentry
  movq %rsp, pe_private_initial_sp(%rip)
  movq %r12, pe_private_mapper_context(%rip)
  cld
  jmp pe_gnu_crt_start
.Lbad_launch:
  leaq .Llaunch_message(%rip), %rdi
  movl $(.Llaunch_end-.Llaunch_message), %esi
  jmp pe_private_crt_fatal
.Lreentry:
  leaq .Lreentry_message(%rip), %rdi
  movl $(.Lreentry_end-.Lreentry_message), %esi
  jmp pe_private_crt_fatal
.size pe_private_crt_entry, .-pe_private_crt_entry

.globl pe_private_crt_fatal
.hidden pe_private_crt_fatal
.type pe_private_crt_fatal,@function
pe_private_crt_fatal:
  movq %rsi, %rdx
  movq %rdi, %rsi
  movl $2, %edi
  movl $1, %eax
  syscall
  movl $127, %edi
  movl $231, %eax
  syscall
  movl $60, %eax
  syscall
.Lexit_refused:
  pause
  jmp .Lexit_refused
.size pe_private_crt_fatal, .-pe_private_crt_fatal

.section .rodata
.Llaunch_message:
  .ascii "private GNU CRT: invalid launch registers\n"
.Llaunch_end:
.Lreentry_message:
  .ascii "private GNU CRT: repeated entry refused\n"
.Lreentry_end:

.section .bss
.balign 8
.globl pe_private_initial_sp
.hidden pe_private_initial_sp
.type pe_private_initial_sp,@object
.size pe_private_initial_sp,8
pe_private_initial_sp:
  .zero 8
.globl pe_private_mapper_context
.hidden pe_private_mapper_context
.type pe_private_mapper_context,@object
.size pe_private_mapper_context,8
pe_private_mapper_context:
  .zero 8
.type pe_private_entry_used,@object
.size pe_private_entry_used,4
pe_private_entry_used:
  .zero 4
.section .note.GNU-stack,"",@progbits
