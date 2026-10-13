/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
#include "state.h"
.text

/* Macro choices expand at assembly time: no feature branch or compiler code
 * lies between the tested instruction and the first architectural snapshots. */
.macro gp_snapshot destination
.set gp_offset,0
.irp reg,rax,rbx,rcx,rdx,rsi,rdi,rbp,rsp,r8,r9,r10,r11,r12,r13,r14,r15
  movq %\reg, \destination+gp_offset(%rip)
.set gp_offset,gp_offset+8
.endr
  pushfq
  popq \destination+STATE_FLAGS(%rip)
  fxsave64 \destination+STATE_FX(%rip)
.endm

.macro vector_load width,n
.if \width == 16
  movdqu vector_seed+(\n*64)(%rip), %xmm\n
.elseif \width == 32
  vmovdqu vector_seed+(\n*64)(%rip), %ymm\n
.else
  vmovdqu64 vector_seed+(\n*64)(%rip), %zmm\n
.endif
.endm

.macro vector_store destination,width,n
.if \width == 16
  movdqu %xmm\n, \destination+STATE_SIMD+(\n*64)(%rip)
.elseif \width == 32
  vmovdqu %ymm\n, \destination+STATE_SIMD+(\n*64)(%rip)
.else
  vmovdqu64 %zmm\n, \destination+STATE_SIMD+(\n*64)(%rip)
.endif
.endm

.macro vectors_seed width,maskwidth
.irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
  vector_load \width,\n
.endr
.if \width == 64
.irp n,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
  vector_load \width,\n
.endr
.irp n,0,1,2,3,4,5,6,7
.if \maskwidth == 8
  kmovq mask_seed+(\n*8)(%rip), %k\n
.else
  kmovw mask_seed+(\n*8)(%rip), %k\n
.endif
.endr
.endif
.endm

.macro vectors_snapshot destination,width,maskwidth
.irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
  vector_store \destination,\width,\n
.endr
.if \width == 64
.irp n,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
  vector_store \destination,\width,\n
.endr
.irp n,0,1,2,3,4,5,6,7
.if \maskwidth == 8
  kmovq %k\n, \destination+STATE_MASK+(\n*8)(%rip)
.else
  kmovw %k\n, \destination+STATE_MASK+(\n*8)(%rip)
.endif
.endr
.endif
.endm

.macro tsc_case name,op,width,maskwidth
.globl \name
.type \name,@function
\name:
.irp reg,rbx,rbp,r12,r13,r14,r15
  pushq %\reg
.endr
  fxsave64 caller_fx(%rip)
  fxrstor64 seed_fx(%rip)
  vectors_seed \width,\maskwidth
  movabsq $0x1122334455667700, %rax
  movabsq $0x1122334455667701, %rbx
  movabsq $0x1122334455667702, %rcx
  movabsq $0x1122334455667703, %rdx
  movabsq $0x1122334455667704, %rsi
  movabsq $0x1122334455667705, %rdi
  movabsq $0x1122334455667706, %rbp
  movabsq $0x1122334455667708, %r8
  movabsq $0x1122334455667709, %r9
  movabsq $0x112233445566770a, %r10
  movabsq $0x112233445566770b, %r11
  movabsq $0x112233445566770c, %r12
  movabsq $0x112233445566770d, %r13
  movabsq $0x112233445566770e, %r14
  movabsq $0x112233445566770f, %r15
  /* CF/PF/AF/ZF/SF/DF/OF set; no privileged flag change. */
  pushq $0xed7
  popfq
  gp_snapshot before_state
  vectors_snapshot before_state,\width,\maskwidth
.globl \name\()_instruction
\name\()_instruction:
.ifc \op,tsc
  .byte 0x66,0x0f,0x31
.else
  .byte 0x66,0x0f,0x01,0xf9
.endif
.globl \name\()_fallthrough
\name\()_fallthrough:
  /* GP MOVs and vector stores leave flags intact; flags are captured before
   * CLD, comparisons, the branch oracle, or any C/library code. */
  gp_snapshot after_state
  vectors_snapshot after_state,\width,\maskwidth
  cld
  incl continuations(%rip)
  cmpl $1, continuations(%rip)
  je .L\name\()_once
  ud2
.L\name\()_once:
  fxrstor64 caller_fx(%rip)
.if \width > 16
  vzeroupper
.endif
.irp reg,r15,r14,r13,r12,rbp,rbx
  popq %\reg
.endr
  ret
.size \name,.-\name
.endm

tsc_case tsc_xmm,tsc,16,0
tsc_case tscp_xmm,tscp,16,0
tsc_case tsc_ymm,tsc,32,0
tsc_case tscp_ymm,tscp,32,0
tsc_case tsc_zmm16,tsc,64,2
tsc_case tscp_zmm16,tscp,64,2
tsc_case tsc_zmm64,tsc,64,8
tsc_case tscp_zmm64,tscp,64,8
.section .note.GNU-stack,"",@progbits
