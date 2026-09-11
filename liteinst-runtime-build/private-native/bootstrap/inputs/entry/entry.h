#ifndef PRIVATE_ENTRY_H
#define PRIVATE_ENTRY_H

#define PE_VERSION 1
#define PE_XSTATE_MASK 0x2e7
#define PE_XSTATE_CAPACITY 4096
#define PE_STACK_SIZE 65536
#define PE_RECORD_SIZE 4416
#define PE_VALID_GPRS 1
#define PE_VALID_XSTATE 2
#define PE_VALID_FS 4
#define PE_VALID_GS 8
#define PE_VALID_POLICY 16
#define PE_VALID_ALL 31
#define PE_FAILURE_CPU 1
#define PE_FAILURE_XSTATE 2
#define PE_FAILURE_FS 3
#define PE_FAILURE_GS 4
#define PE_FAILURE_SHSTK 5
#define PE_FAILURE_RETURN 6
#define PE_FAILURE_STACK 7

#define PE_VERSION_OFF 0
#define PE_SIZE_OFF 8
#define PE_VALID_OFF 16
#define PE_FAILURE_OFF 24
#define PE_ERROR_OFF 32
#define PE_RAX 40
#define PE_RBX 48
#define PE_RCX 56
#define PE_RDX 64
#define PE_RSI 72
#define PE_RDI 80
#define PE_RBP 88
#define PE_RSP 96
#define PE_R8 104
#define PE_R9 112
#define PE_R10 120
#define PE_R11 128
#define PE_R12 136
#define PE_R13 144
#define PE_R14 152
#define PE_R15 160
#define PE_FLAGS 168
#define PE_PC 176
#define PE_FS_BASE 184
#define PE_GS_BASE 192
#define PE_CS 200
#define PE_SS 202
#define PE_DS 204
#define PE_ES 206
#define PE_FS 208
#define PE_GS 210
#define PE_XCR0 216
#define PE_XSTATE_SIZE 224
#define PE_CPU_MASK 232
#define PE_SHSTK 240
#define PE_STORAGE_BEGIN 248
#define PE_STORAGE_END 256
#define PE_STACK_BEGIN 264
#define PE_STACK_END 272
#define PE_SHSTK_RESULT 280
#define PE_XSTATE 320

#ifndef __ASSEMBLER__
#include <stddef.h>
#include <stdint.h>
#include "private_startup.h"

struct pe_entry {
  uint64_t version, size, valid, failure;
  int64_t raw_error;
  uint64_t rax, rbx, rcx, rdx, rsi, rdi, rbp, rsp;
  uint64_t r8, r9, r10, r11, r12, r13, r14, r15;
  uint64_t rflags_image, entry_pc, fs_base, gs_base;
  uint16_t cs, ss, ds, es, fs, gs;
  uint32_t reserved;
  uint64_t xcr0, xstate_size, cpu_user_mask, shstk;
  uintptr_t storage_begin, storage_end, scratch_begin, scratch_end;
  int64_t shstk_result;
  unsigned char reserved_tail[32];
  _Alignas(64) unsigned char xstate[PE_XSTATE_CAPACITY];
};

struct pe_mapped_inputs {
  struct ps_view original_stack;
  struct ps_view runtime_file;
  uintptr_t runtime_bias;
  size_t runtime_mapping_size;
  uintptr_t crt_entry_offset;
  struct ps_view tool_name;
  size_t call_stack_reserve;
};

_Static_assert(sizeof(struct pe_entry) == PE_RECORD_SIZE, "entry record size");
_Static_assert(_Alignof(struct pe_entry) == 64, "entry record alignment");
#define PE_OFFSET(field, value) \
  _Static_assert(offsetof(struct pe_entry, field) == value, #field " offset")
PE_OFFSET(version, PE_VERSION_OFF);
PE_OFFSET(size, PE_SIZE_OFF);
PE_OFFSET(valid, PE_VALID_OFF);
PE_OFFSET(failure, PE_FAILURE_OFF);
PE_OFFSET(raw_error, PE_ERROR_OFF);
PE_OFFSET(rax, PE_RAX);
PE_OFFSET(rbx, PE_RBX);
PE_OFFSET(rcx, PE_RCX);
PE_OFFSET(rdx, PE_RDX);
PE_OFFSET(rsi, PE_RSI);
PE_OFFSET(rdi, PE_RDI);
PE_OFFSET(rbp, PE_RBP);
PE_OFFSET(rsp, PE_RSP);
PE_OFFSET(r8, PE_R8);
PE_OFFSET(r9, PE_R9);
PE_OFFSET(r10, PE_R10);
PE_OFFSET(r11, PE_R11);
PE_OFFSET(r12, PE_R12);
PE_OFFSET(r13, PE_R13);
PE_OFFSET(r14, PE_R14);
PE_OFFSET(r15, PE_R15);
PE_OFFSET(rflags_image, PE_FLAGS);
PE_OFFSET(entry_pc, PE_PC);
PE_OFFSET(fs_base, PE_FS_BASE);
PE_OFFSET(gs_base, PE_GS_BASE);
PE_OFFSET(cs, PE_CS);
PE_OFFSET(ss, PE_SS);
PE_OFFSET(ds, PE_DS);
PE_OFFSET(es, PE_ES);
PE_OFFSET(fs, PE_FS);
PE_OFFSET(gs, PE_GS);
PE_OFFSET(xcr0, PE_XCR0);
PE_OFFSET(xstate_size, PE_XSTATE_SIZE);
PE_OFFSET(cpu_user_mask, PE_CPU_MASK);
PE_OFFSET(shstk, PE_SHSTK);
PE_OFFSET(storage_begin, PE_STORAGE_BEGIN);
PE_OFFSET(storage_end, PE_STORAGE_END);
PE_OFFSET(scratch_begin, PE_STACK_BEGIN);
PE_OFFSET(scratch_end, PE_STACK_END);
PE_OFFSET(shstk_result, PE_SHSTK_RESULT);
PE_OFFSET(xstate, PE_XSTATE);
#undef PE_OFFSET

__attribute__((visibility("hidden"), noreturn))
void pe_continue(const struct pe_entry *entry);

enum ps_status pe_builder_request(const struct pe_entry *entry,
                                 const struct pe_mapped_inputs *inputs,
                                 struct ps_request *request);
#endif
#endif
