#ifndef PRIVATE_STARTUP_H
#define PRIVATE_STARTUP_H

#include <stddef.h>
#include <stdint.h>

#define PS_ABI_VERSION UINT64_C(1)
#define PS_AUX_HANDOFF UINT64_C(0x6fff5053)
#define PS_MAX_VECTOR 65536u
#define PS_MAX_AUX 128u
#define PS_MAX_STACK (64u * 1024u * 1024u)

enum ps_status {
  PS_OK, PS_ARGUMENT, PS_STACK, PS_AUX, PS_UNSUPPORTED, PS_IMAGE,
  PS_OVERFLOW, PS_SPACE, PS_OVERLAP
};

struct ps_view { const unsigned char *data; size_t size; };

struct ps_request {
  struct ps_view guest_stack;
  struct ps_view image_file;
  uintptr_t image_bias;
  size_t image_mapping_size;
  uintptr_t crt_entry_offset;
  struct ps_view tool_name;
  size_t call_stack_reserve;
};

struct ps_handoff {
  uint64_t version;
  uint64_t size;
  uintptr_t original_sp;
  uintptr_t original_stack_end;
  uintptr_t original_argv;
  uint64_t original_argc;
  uintptr_t original_envp;
  uint64_t original_envc;
  uintptr_t original_auxv;
  uint64_t original_auxc;
  uintptr_t original_random;
  uintptr_t private_stack_begin;
  uintptr_t private_stack_end;
};

struct ps_launch {
  uintptr_t crt_entry;
  uintptr_t crt_sp;
  uintptr_t crt_rdx;
  const struct ps_handoff *handoff;
};

enum ps_status ps_measure(const struct ps_request *, size_t *);
enum ps_status ps_build(const struct ps_request *, void *, size_t,
                        struct ps_launch *);

#endif
