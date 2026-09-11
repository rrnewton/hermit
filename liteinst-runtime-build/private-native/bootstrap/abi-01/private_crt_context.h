#ifndef PRIVATE_CRT_CONTEXT_H
#define PRIVATE_CRT_CONTEXT_H
#include <stddef.h>
#include <stdint.h>

#define PM_CONTEXT_VERSION 1u

struct pe_entry;
struct ps_handoff;

struct pm_crt_context {
  uint64_t version, size;
  const struct pe_entry *captured;
  const struct ps_handoff *stack_handoff;
  uintptr_t runtime_bias;
  size_t runtime_span;
  const void *owner;
};

_Static_assert(sizeof(struct pm_crt_context) == 56, "context size");
_Static_assert(_Alignof(struct pm_crt_context) == 8, "context alignment");
_Static_assert(offsetof(struct pm_crt_context, version) == 0, "version offset");
_Static_assert(offsetof(struct pm_crt_context, size) == 8, "size offset");
_Static_assert(offsetof(struct pm_crt_context, captured) == 16, "captured offset");
_Static_assert(offsetof(struct pm_crt_context, stack_handoff) == 24, "handoff offset");
_Static_assert(offsetof(struct pm_crt_context, runtime_bias) == 32, "bias offset");
_Static_assert(offsetof(struct pm_crt_context, runtime_span) == 40, "span offset");
_Static_assert(offsetof(struct pm_crt_context, owner) == 48, "owner offset");

__attribute__((visibility("hidden"), noreturn))
void pm_enter_private_crt(const struct pm_crt_context *context,
                          uintptr_t crt_entry, uintptr_t crt_sp);
#endif
