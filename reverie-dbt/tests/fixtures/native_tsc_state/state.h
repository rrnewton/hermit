/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
#ifndef NATIVE_TSC_STATE_H
#define NATIVE_TSC_STATE_H

/* Shared offsets are asserted by C. No packed or unaligned FP/SIMD storage. */
#define STATE_GPR 0
#define STATE_FLAGS 128
#define STATE_FX 192
#define STATE_SIMD 704
#define STATE_MASK 2752
#define STATE_SIZE 2816
#define TSC_WORD 0x1234567823456789
#define TSC_AUX 0xd15ea5e1

#ifndef __ASSEMBLER__
#include <stdint.h>
struct __attribute__((aligned(64))) state_image {
  uint64_t gpr[16]; /* rax,rbx,rcx,rdx,rsi,rdi,rbp,rsp,r8,...,r15 */
  uint64_t flags;
  unsigned char padding[56];
  unsigned char fx[512];
  unsigned char simd[32][64];
  uint64_t mask[8];
};
extern struct state_image before_state, after_state;
extern unsigned char caller_fx[512], seed_fx[512];
extern uint64_t vector_seed[32][8], mask_seed[8];
extern uint32_t continuations;
void tsc_xmm(void);
void tscp_xmm(void);
void tsc_ymm(void);
void tscp_ymm(void);
void tsc_zmm16(void);
void tscp_zmm16(void);
void tsc_zmm64(void);
void tscp_zmm64(void);
#endif
#endif
