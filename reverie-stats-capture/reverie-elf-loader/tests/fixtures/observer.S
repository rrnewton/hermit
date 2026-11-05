/* This freestanding ET_DYN binary is the dummy program's PT_INTERP.
 * The first 16 instructions save the complete original GPR set using
 * RIP-relative stores.  Even PUSHFQ uses a separate BSS stack, so none
 * of the original stack bytes that follow the header are overwritten.
 * The register header's layout is documented in README.md.
 */
	.set HEADER_SIZE, 2368
	.set STACK_CAP, 262144
	.bss
	.balign 64
report:
	.space HEADER_SIZE + STACK_CAP
	.balign 16
private_stack:
	.space 8192
private_stack_end:

	.text
	.globl _start
	.type _start,@function
_start:
	mov %rax, report+16(%rip)
	mov %rbx, report+24(%rip)
	mov %rcx, report+32(%rip)
	mov %rdx, report+40(%rip)
	mov %rsi, report+48(%rip)
	mov %rdi, report+56(%rip)
	mov %rbp, report+64(%rip)
	mov %rsp, report+72(%rip)
	mov %r8, report+80(%rip)
	mov %r9, report+88(%rip)
	mov %r10, report+96(%rip)
	mov %r11, report+104(%rip)
	mov %r12, report+112(%rip)
	mov %r13, report+120(%rip)
	mov %r14, report+128(%rip)
	mov %r15, report+136(%rip)
	lea private_stack_end(%rip), %rsp
	pushfq
	popq report+144(%rip)
	stmxcsr report+152(%rip)
	/* FXSAVE is read-only with respect to the live x87/SIMD registers.
	 * Reuse the bottom of our private stack for its aligned 512-byte image;
	 * this neither expands BSS nor writes to the original initial stack. */
	fxsave64 private_stack(%rip)
	movw private_stack(%rip), %ax
	movw %ax, report+156(%rip) /* FCW */
	movw private_stack+2(%rip), %ax
	movw %ax, report+158(%rip) /* FSW, including TOP and condition codes */
	movb private_stack+4(%rip), %al
	movb %al, report+2308(%rip) /* abridged FTW */
	movw %cs, report+2310(%rip)
	movw %ss, report+2312(%rip)
	movw %ds, report+2314(%rip)
	movw %es, report+2316(%rip)
	movw %fs, report+2318(%rip)
	movw %gs, report+2320(%rip)
	movdqu %xmm0, report+192(%rip)
	movdqu %xmm1, report+208(%rip)
	movdqu %xmm2, report+224(%rip)
	movdqu %xmm3, report+240(%rip)
	movdqu %xmm4, report+256(%rip)
	movdqu %xmm5, report+272(%rip)
	movdqu %xmm6, report+288(%rip)
	movdqu %xmm7, report+304(%rip)
	movdqu %xmm8, report+320(%rip)
	movdqu %xmm9, report+336(%rip)
	movdqu %xmm10, report+352(%rip)
	movdqu %xmm11, report+368(%rip)
	movdqu %xmm12, report+384(%rip)
	movdqu %xmm13, report+400(%rip)
	movdqu %xmm14, report+416(%rip)
	movdqu %xmm15, report+432(%rip)
	/* Unlike ptrace GETREGS, these independent witnesses query the live
	 * task's bases. ARCH_GET_FS/GS do not change either base or selector. */
	mov $158, %eax /* arch_prctl */
	mov $0x1003, %edi /* ARCH_GET_FS */
	lea report+2328(%rip), %rsi
	syscall
	test %rax, %rax
	jne .Lbad_registers
	mov $158, %eax
	mov $0x1004, %edi /* ARCH_GET_GS */
	lea report+2336(%rip), %rsi
	syscall
	test %rax, %rax
	jne .Lbad_registers

	movabs $0x313053424f464c45, %rax /* "ELFOBS01" */
	mov %rax, report(%rip)
	movq $HEADER_SIZE, report+8(%rip)
	xor %eax, %eax
	cpuid
	mov %eax, %r14d
	mov $1, %eax
	cpuid
	bt $27, %ecx /* OSXSAVE */
	jnc .Lextended_done
	mov %ecx, %r15d
	xor %ecx, %ecx
	xgetbv
	shl $32, %rdx
	or %rdx, %rax
	mov %rax, report+168(%rip)
	mov %rax, %r13
	bt $28, %r15d /* AVX */
	jnc .Lcheck_leaf7
	and $6, %eax /* SSE and AVX state enabled */
	cmp $6, %eax
	jne .Lcheck_leaf7
	orq $1, report+160(%rip)
	vextractf128 $1, %ymm0, report+448(%rip)
	vextractf128 $1, %ymm1, report+464(%rip)
	vextractf128 $1, %ymm2, report+480(%rip)
	vextractf128 $1, %ymm3, report+496(%rip)
	vextractf128 $1, %ymm4, report+512(%rip)
	vextractf128 $1, %ymm5, report+528(%rip)
	vextractf128 $1, %ymm6, report+544(%rip)
	vextractf128 $1, %ymm7, report+560(%rip)
	vextractf128 $1, %ymm8, report+576(%rip)
	vextractf128 $1, %ymm9, report+592(%rip)
	vextractf128 $1, %ymm10, report+608(%rip)
	vextractf128 $1, %ymm11, report+624(%rip)
	vextractf128 $1, %ymm12, report+640(%rip)
	vextractf128 $1, %ymm13, report+656(%rip)
	vextractf128 $1, %ymm14, report+672(%rip)
	vextractf128 $1, %ymm15, report+688(%rip)
.Lcheck_leaf7:
	cmp $7, %r14d
	jb .Lextended_done
	mov $7, %eax
	xor %ecx, %ecx
	cpuid
	mov %ecx, %r15d
	bt $16, %ebx /* AVX512F */
	jnc .Lcheck_pkru
	mov %r13, %rax
	and $0xe6, %eax /* SSE/AVX/opmask/ZMM state enabled */
	cmp $0xe6, %eax
	jne .Lcheck_pkru
	orq $2, report+160(%rip)
	vextractf64x4 $1, %zmm0, report+704(%rip)
	vextractf64x4 $1, %zmm1, report+736(%rip)
	vextractf64x4 $1, %zmm2, report+768(%rip)
	vextractf64x4 $1, %zmm3, report+800(%rip)
	vextractf64x4 $1, %zmm4, report+832(%rip)
	vextractf64x4 $1, %zmm5, report+864(%rip)
	vextractf64x4 $1, %zmm6, report+896(%rip)
	vextractf64x4 $1, %zmm7, report+928(%rip)
	vextractf64x4 $1, %zmm8, report+960(%rip)
	vextractf64x4 $1, %zmm9, report+992(%rip)
	vextractf64x4 $1, %zmm10, report+1024(%rip)
	vextractf64x4 $1, %zmm11, report+1056(%rip)
	vextractf64x4 $1, %zmm12, report+1088(%rip)
	vextractf64x4 $1, %zmm13, report+1120(%rip)
	vextractf64x4 $1, %zmm14, report+1152(%rip)
	vextractf64x4 $1, %zmm15, report+1184(%rip)
	vmovdqu64 %zmm16, report+1216(%rip)
	vmovdqu64 %zmm17, report+1280(%rip)
	vmovdqu64 %zmm18, report+1344(%rip)
	vmovdqu64 %zmm19, report+1408(%rip)
	vmovdqu64 %zmm20, report+1472(%rip)
	vmovdqu64 %zmm21, report+1536(%rip)
	vmovdqu64 %zmm22, report+1600(%rip)
	vmovdqu64 %zmm23, report+1664(%rip)
	vmovdqu64 %zmm24, report+1728(%rip)
	vmovdqu64 %zmm25, report+1792(%rip)
	vmovdqu64 %zmm26, report+1856(%rip)
	vmovdqu64 %zmm27, report+1920(%rip)
	vmovdqu64 %zmm28, report+1984(%rip)
	vmovdqu64 %zmm29, report+2048(%rip)
	vmovdqu64 %zmm30, report+2112(%rip)
	vmovdqu64 %zmm31, report+2176(%rip)
	bt $30, %ebx /* AVX512BW enables the 64-bit KMOVQ operands */
	jnc .Lnarrow_opmask
	orq $8, report+160(%rip)
	kmovq %k0, report+2240(%rip)
	kmovq %k1, report+2248(%rip)
	kmovq %k2, report+2256(%rip)
	kmovq %k3, report+2264(%rip)
	kmovq %k4, report+2272(%rip)
	kmovq %k5, report+2280(%rip)
	kmovq %k6, report+2288(%rip)
	kmovq %k7, report+2296(%rip)
	jmp .Lcheck_pkru
.Lnarrow_opmask:
	kmovw %k0, report+2240(%rip)
	kmovw %k1, report+2248(%rip)
	kmovw %k2, report+2256(%rip)
	kmovw %k3, report+2264(%rip)
	kmovw %k4, report+2272(%rip)
	kmovw %k5, report+2280(%rip)
	kmovw %k6, report+2288(%rip)
	kmovw %k7, report+2296(%rip)
.Lcheck_pkru:
	mov %r15d, %eax
	and $0x18, %eax /* PKU and OSPKE */
	cmp $0x18, %eax
	jne .Lextended_done
	xor %ecx, %ecx
	rdpkru
	mov %eax, report+2304(%rip)
	orq $4, report+160(%rip)
.Lextended_done:
	/* Find the upper bound of the kernel's populated initial stack. */
	mov report+72(%rip), %r8
	mov (%r8), %rcx
	lea 16(%r8,%rcx,8), %rsi
.Lenv:
	cmpq $0, (%rsi)
	lea 8(%rsi), %rsi
	jne .Lenv
.Laux:
	mov (%rsi), %rax
	cmp $31, %rax
	je .Lexecfn
	test %rax, %rax
	je .Lbad_stack
	add $16, %rsi
	jmp .Laux
.Lexecfn:
	mov 8(%rsi), %rsi
.Lend:
	cmpb $0, (%rsi)
	lea 1(%rsi), %rsi
	jne .Lend
	add $4095, %rsi
	and $-4096, %rsi
	sub $1024, %r8
	mov %r8, report+176(%rip)
	sub %r8, %rsi
	cmp $STACK_CAP, %rsi
	ja .Lbad_stack
	mov %rsi, report+184(%rip)
	mov %rsi, %rcx
	mov %r8, %rsi
	lea report+HEADER_SIZE(%rip), %rdi
	cld
	rep movsb
	lea report(%rip), %r12
	mov report+184(%rip), %r13
	add $HEADER_SIZE, %r13
.Lwrite:
	mov $1, %eax
	mov $1, %edi
	mov %r12, %rsi
	mov %r13, %rdx
	syscall
	test %rax, %rax
	jle .Lbad_write
	add %rax, %r12
	sub %rax, %r13
	jnz .Lwrite
	xor %edi, %edi
	jmp .Lexit
.Lbad_write:
	mov $97, %edi
	jmp .Lexit
.Lbad_stack:
	mov $98, %edi
	jmp .Lexit
.Lbad_registers:
	mov $96, %edi
.Lexit:
	mov $231, %eax
	syscall
	ud2
	.size _start, .-_start
	.section .note.GNU-stack,"",@progbits
