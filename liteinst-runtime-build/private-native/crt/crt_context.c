#include "crt_context.h"

_Static_assert(sizeof(struct ps_handoff) == 104, "handoff ABI");
_Static_assert(sizeof(struct pm_crt_context) == 56, "mapper context ABI");

static int contains(uintptr_t begin, uintptr_t end, uintptr_t address,
                    uint64_t count, size_t width) {
  return begin < end && address >= begin && address <= end &&
         count <= (end - address) / width;
}

enum pc_context_status pc_context_init(struct pc_runtime_context *output,
                                     const struct ps_handoff *handoff,
                                     uintptr_t initial_sp, int argc,
                                     char **argv, char **envp) {
  if (!output || !handoff || !argv || !envp || argc != 1)
    return PC_CONTEXT_ARGUMENT;
  if (handoff->version != PS_ABI_VERSION || handoff->size != sizeof(*handoff))
    return PC_CONTEXT_VERSION;
  uintptr_t private_begin = handoff->private_stack_begin;
  uintptr_t private_end = handoff->private_stack_end;
  if (private_begin >= private_end || private_end - private_begin > PS_MAX_STACK ||
      (initial_sp & 15) ||
      !contains(private_begin, private_end, initial_sp, 1, sizeof(uintptr_t)) ||
      !contains(private_begin, private_end, (uintptr_t)handoff, 1, sizeof(*handoff)) ||
      !contains(private_begin, private_end, (uintptr_t)argv, 2, sizeof(uintptr_t)) ||
      (uintptr_t)argv != initial_sp + sizeof(uintptr_t) ||
      (uintptr_t)envp != (uintptr_t)argv + 2 * sizeof(uintptr_t) ||
      handoff->original_envc > PS_MAX_VECTOR ||
      !contains(private_begin, private_end, (uintptr_t)envp,
                handoff->original_envc + 1, sizeof(uintptr_t)))
    return PC_CONTEXT_PRIVATE_STACK;
  uintptr_t original_begin = handoff->original_sp;
  uintptr_t original_end = handoff->original_stack_end;
  if (original_begin >= original_end || original_end - original_begin > PS_MAX_STACK ||
      (original_begin & 7) || handoff->original_argc > PS_MAX_VECTOR ||
      handoff->original_auxc > PS_MAX_AUX ||
      !contains(original_begin, original_end, original_begin, 1, sizeof(uintptr_t)) ||
      !contains(original_begin, original_end, handoff->original_argv,
                handoff->original_argc + 1, sizeof(uintptr_t)) ||
      handoff->original_argv != original_begin + sizeof(uintptr_t) ||
      !contains(original_begin, original_end, handoff->original_envp,
                handoff->original_envc + 1, sizeof(uintptr_t)) ||
      handoff->original_envp != handoff->original_argv +
                                   (handoff->original_argc + 1) * sizeof(uintptr_t) ||
      !contains(original_begin, original_end, handoff->original_auxv,
                handoff->original_auxc + 1, 2 * sizeof(uintptr_t)) ||
      handoff->original_auxv != handoff->original_envp +
                                   (handoff->original_envc + 1) * sizeof(uintptr_t) ||
      !contains(original_begin, original_end, handoff->original_random, 16, 1))
    return PC_CONTEXT_ORIGINAL_STACK;
  if (private_begin < original_end && original_begin < private_end)
    return PC_CONTEXT_OVERLAP;
  *output = (struct pc_runtime_context){*handoff, initial_sp, argc, argv, envp};
  return PC_CONTEXT_OK;
}

enum pc_context_status pc_mapper_context_validate(const struct pm_crt_context *context,
                                                 const struct ps_handoff *handoff,
                                                 uintptr_t image_begin,
                                                 uintptr_t crt_entry) {
  if (!context || (uintptr_t)context % _Alignof(struct pm_crt_context) ||
      !handoff || context->version != PM_VERSION || context->size != sizeof(*context) ||
      context->stack_handoff != handoff || !context->owner || !context->captured ||
      (uintptr_t)context->captured % _Alignof(struct pe_entry))
    return PC_CONTEXT_MAPPER;
  const struct pe_entry *captured = context->captured;
  if (captured->version != PE_VERSION || captured->size != sizeof(*captured) ||
      captured->valid != PE_VALID_ALL || captured->failure || captured->raw_error ||
      captured->rsp != handoff->original_sp || captured->shstk ||
      (captured->shstk_result != 0 && captured->shstk_result != -22) ||
      (captured->xcr0 & ~(uint64_t)PE_XSTATE_MASK) || (captured->xcr0 & 3) != 3 ||
      (captured->cpu_user_mask & captured->xcr0) != captured->xcr0 ||
      captured->xstate_size < 576 || captured->xstate_size > PE_XSTATE_CAPACITY ||
      !captured->storage_begin || (captured->storage_begin & 63) ||
      captured->storage_begin > UINTPTR_MAX - sizeof(*captured) ||
      captured->scratch_begin < captured->storage_begin + sizeof(*captured) ||
      captured->scratch_begin > UINTPTR_MAX - PE_STACK_SIZE ||
      (captured->scratch_begin & (PM_PAGE - 1)) ||
      captured->scratch_end != captured->scratch_begin + PE_STACK_SIZE ||
      captured->storage_end != captured->scratch_end)
    return PC_CONTEXT_CAPTURE;
  if (!image_begin || image_begin % PM_PAGE || context->runtime_bias != image_begin ||
      !context->runtime_span || context->runtime_span % PM_PAGE ||
      context->runtime_span > PM_MAX_SPAN ||
      context->runtime_span > UINTPTR_MAX - image_begin ||
      !contains(image_begin, image_begin + context->runtime_span, crt_entry, 1, 1))
    return PC_CONTEXT_IMAGE;
  return PC_CONTEXT_OK;
}
