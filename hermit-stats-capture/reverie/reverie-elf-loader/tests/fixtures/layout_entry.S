/* Preserve the kernel-built initial stack before the C runtime starts.
 * No instruction writes to the original stack.  The dynamic linker has
 * already used the region below RSP; observer.S records that region at
 * the interpreter's actual first instruction instead.
 */
	.bss
	.balign 16
	.globl fixture_entry_rsp
fixture_entry_rsp:
	.quad 0
	.globl fixture_stack_len
fixture_stack_len:
	.quad 0
	.globl fixture_stack
fixture_stack:
	.space 262144

	.text
	.globl fixture_start
	.type fixture_start,@function
fixture_start:
	mov %rsp, fixture_entry_rsp(%rip)
	/* Walk argc, argv, envp, and auxv without changing ld.so's RDX. */
	mov (%rsp), %rax
	lea 16(%rsp,%rax,8), %rsi
1:
	cmpq $0, (%rsi)
	lea 8(%rsi), %rsi
	jne 1b
2:
	mov (%rsi), %rax
	cmp $31, %rax /* AT_EXECFN */
	je 3f
	test %rax, %rax
	je 9f
	add $16, %rsi
	jmp 2b
3:
	mov 8(%rsi), %rsi
4:
	cmpb $0, (%rsi)
	lea 1(%rsi), %rsi
	jne 4b
	/* Include the trailing bytes of the top populated stack page. */
	add $4095, %rsi
	and $-4096, %rsi
	sub %rsp, %rsi
	cmp $262144, %rsi
	ja 9f
	mov %rsi, fixture_stack_len(%rip)
	mov %rsi, %rcx
	mov %rsp, %rsi
	lea fixture_stack(%rip), %rdi
	cld
	rep movsb
	jmp _start
9:
	mov $231, %eax
	mov $98, %edi
	syscall
	ud2
	.size fixture_start, .-fixture_start
	.section .note.GNU-stack,"",@progbits
