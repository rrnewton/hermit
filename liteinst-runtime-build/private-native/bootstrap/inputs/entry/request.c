#include "entry.h"

static int interval(uintptr_t begin, size_t size, uintptr_t *end) {
  if (begin == 0 || size == 0 || size > UINTPTR_MAX - begin)
    return 0;
  *end = begin + size;
  return 1;
}

static int overlaps(uintptr_t begin, uintptr_t end,
                    uintptr_t other_begin, uintptr_t other_end) {
  return begin < other_end && end > other_begin;
}

enum ps_status pe_builder_request(const struct pe_entry *entry,
                                 const struct pe_mapped_inputs *inputs,
                                 struct ps_request *request) {
  uintptr_t stack_end, output_end, file_end, name_end;
  if (entry == NULL || inputs == NULL || request == NULL)
    return PS_ARGUMENT;
  if (entry->version != PE_VERSION || entry->size != sizeof(*entry) ||
      entry->valid != PE_VALID_ALL || entry->failure != 0 ||
      entry->raw_error != 0 || entry->shstk != 0 ||
      (entry->shstk_result != 0 && entry->shstk_result != -22) ||
      (entry->xcr0 & ~(uint64_t)PE_XSTATE_MASK) != 0 ||
      (entry->xcr0 & 3) != 3 ||
      (entry->cpu_user_mask & entry->xcr0) != entry->xcr0 ||
      entry->xstate_size < 576 || entry->xstate_size > PE_XSTATE_CAPACITY)
    return PS_UNSUPPORTED;
  if (entry->storage_begin != (uintptr_t)entry ||
      entry->storage_begin > UINTPTR_MAX - sizeof(*entry) ||
      entry->scratch_begin < entry->storage_begin + sizeof(*entry) ||
      entry->scratch_begin > UINTPTR_MAX - PE_STACK_SIZE ||
      entry->scratch_end != entry->scratch_begin + PE_STACK_SIZE ||
      entry->storage_end != entry->scratch_end ||
      (entry->storage_begin & 63) != 0 ||
      (entry->scratch_begin & 4095) != 0)
    return PS_ARGUMENT;
  if ((uintptr_t)inputs->original_stack.data != entry->rsp ||
      (entry->rsp & 15) != 0 ||
      inputs->original_stack.size > PS_MAX_STACK ||
      !interval(entry->rsp, inputs->original_stack.size, &stack_end))
    return PS_STACK;
  if (entry->rsp < entry->storage_end && stack_end > entry->storage_begin)
    return PS_OVERLAP;
  if (!interval((uintptr_t)request, sizeof(*request), &output_end) ||
      !interval((uintptr_t)inputs->runtime_file.data,
                inputs->runtime_file.size, &file_end) ||
      !interval((uintptr_t)inputs->tool_name.data,
                inputs->tool_name.size, &name_end))
    return PS_ARGUMENT;
  if (overlaps((uintptr_t)request, output_end, entry->rsp, stack_end) ||
      overlaps((uintptr_t)request, output_end,
               entry->storage_begin, entry->storage_begin + sizeof(*entry)) ||
      overlaps((uintptr_t)request, output_end,
               (uintptr_t)inputs->runtime_file.data, file_end) ||
      overlaps((uintptr_t)request, output_end,
               (uintptr_t)inputs->tool_name.data, name_end) ||
      (uintptr_t)inputs > UINTPTR_MAX - sizeof(*inputs) ||
      overlaps((uintptr_t)request, output_end,
               (uintptr_t)inputs, (uintptr_t)inputs + sizeof(*inputs)))
    return PS_OVERLAP;
  struct ps_request prepared = {
    .guest_stack = inputs->original_stack,
    .image_file = inputs->runtime_file,
    .image_bias = inputs->runtime_bias,
    .image_mapping_size = inputs->runtime_mapping_size,
    .crt_entry_offset = inputs->crt_entry_offset,
    .tool_name = inputs->tool_name,
    .call_stack_reserve = inputs->call_stack_reserve
  };
  *request = prepared;
  return PS_OK;
}
