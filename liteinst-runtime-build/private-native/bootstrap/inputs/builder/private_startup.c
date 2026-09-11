#include "private_startup.h"

#if !defined(__x86_64__) || defined(__ILP32__)
#error "private startup requires x86-64 LP64"
#endif

_Static_assert(sizeof(uintptr_t) == 8 && sizeof(size_t) == 8, "x86-64 ABI");
_Static_assert(sizeof(struct ps_handoff) == 104, "handoff ABI size");

struct decoded {
  size_t argc, env_offset, envc, aux_offset, auxc, payload;
  uintptr_t phdr, entry, random;
  size_t phnum;
};

static int range(uintptr_t base, size_t size) { return size <= UINTPTR_MAX - base; }
static int inside(size_t offset, size_t size, size_t limit)
{ return offset <= limit && size <= limit - offset; }
static int overlap(uintptr_t first, size_t size, uintptr_t other, size_t count)
{ return size && count && first < other + count && other < first + size; }
static int add(size_t *value, size_t amount)
{
  if (amount > SIZE_MAX - *value) return 0;
  *value += amount;
  return 1;
}
static uint64_t read64(const unsigned char *bytes)
{
  uint64_t value = 0;
  for (unsigned int shift = 0; shift != 64; shift += 8)
    value |= (uint64_t) *bytes++ << shift;
  return value;
}
static uint32_t read32(const unsigned char *bytes)
{ return (uint32_t) bytes[0] | (uint32_t) bytes[1] << 8 |
         (uint32_t) bytes[2] << 16 | (uint32_t) bytes[3] << 24; }
static uint16_t read16(const unsigned char *bytes)
{ return (uint16_t) bytes[0] | (uint16_t) bytes[1] << 8; }
static void write64(unsigned char *bytes, uint64_t value)
{
  for (unsigned int shift = 0; shift != 64; shift += 8)
    *bytes++ = (unsigned char) (value >> shift);
}
static void copy(unsigned char *to, const unsigned char *from, size_t size)
{ for (size_t index = 0; index < size; ++index) to[index] = from[index]; }
static int power2(uint64_t value) { return value && !(value & (value - 1)); }

static int string_size(struct ps_view view, uintptr_t pointer, size_t *size)
{
  uintptr_t base = (uintptr_t) view.data;
  if (pointer < base || pointer - base >= view.size) return 0;
  size_t offset = pointer - base;
  for (size_t length = 0; length < view.size - offset; ++length)
    if (!view.data[offset + length]) { *size = length + 1; return 1; }
  return 0;
}
static int checked_string(struct ps_view view, uintptr_t pointer, size_t *size,
                          size_t *remaining)
{
  if (!string_size(view, pointer, size) || *size > *remaining) return 0;
  *remaining -= *size;
  return 1;
}

static enum ps_status image(const struct ps_request *request, struct decoded *state)
{
  struct ps_view file = request->image_file;
  if (file.size < 64) return PS_IMAGE;
  const unsigned char *header = file.data;
  if (read32(header) != UINT32_C(0x464c457f) || header[4] != 2 ||
      header[5] != 1 || header[6] != 1 || (header[7] != 0 && header[7] != 3) ||
      header[8] || read16(header + 16) != 3 || read16(header + 18) != 62 ||
      read32(header + 20) != 1 || read32(header + 48) ||
      read16(header + 52) != 64 || read16(header + 54) != 56)
    return PS_IMAGE;
  size_t phoff = read64(header + 32), phnum = read16(header + 56);
  uint64_t entry = read64(header + 24);
  if (!phnum || phnum > PS_MAX_AUX || phoff < 64 || phoff % 8 || !entry ||
      !request->crt_entry_offset ||
      !inside(phoff, phnum * 56, file.size) ||
      !request->image_bias || request->image_bias % 4096 ||
      !range(request->image_bias, request->image_mapping_size)) return PS_IMAGE;
  unsigned int header_load = 0, entry_load = 0, crt_load = 0, dynamic = 0, tls = 0;
  for (size_t index = 0; index < phnum; ++index) {
    const unsigned char *phdr = file.data + phoff + index * 56;
    uint32_t type = read32(phdr), flags = read32(phdr + 4);
    uint64_t offset = read64(phdr + 8), address = read64(phdr + 16);
    uint64_t filesz = read64(phdr + 32), memsz = read64(phdr + 40);
    uint64_t alignment = read64(phdr + 48);
    if (type == 3) return PS_UNSUPPORTED;
    if (type != 1 && type != 2 && type != 7) continue;
    if (filesz > memsz || !inside(offset, filesz, file.size) ||
        !inside(address, memsz, request->image_mapping_size) ||
        (alignment > 1 && (!power2(alignment) ||
          (address % alignment != offset % alignment)))) return PS_IMAGE;
    if (type == 1) {
      if ((flags & ~7u) || !(flags & 4u) || (flags & 3u) == 3u ||
          (alignment > 1 && request->image_bias % alignment)) return PS_IMAGE;
      if (!offset && !address && inside(phoff, phnum * 56, filesz)) ++header_load;
      if ((flags & 1u) && entry >= address && entry - address < filesz) ++entry_load;
      if ((flags & 1u) && request->crt_entry_offset >= address &&
          request->crt_entry_offset - address < filesz) ++crt_load;
    } else if (type == 2) {
      if (!filesz || filesz % 16 || address % 8 || offset % 8 || ++dynamic != 1)
        return PS_IMAGE;
      int terminated = 0;
      for (size_t cursor = 0; cursor < filesz; cursor += 16) {
        uint64_t tag = read64(file.data + offset + cursor);
        if (!tag) { terminated = 1; break; }
        if (tag == 1) return PS_UNSUPPORTED;
      }
      if (!terminated) return PS_IMAGE;
    } else if (++tls != 1) return PS_IMAGE;
  }
  if (header_load != 1 || entry_load != 1 || crt_load != 1 || dynamic != 1 || tls != 1)
    return PS_IMAGE;
  for (size_t index = 0; index < phnum; ++index) {
    const unsigned char *part = file.data + phoff + index * 56;
    uint32_t type = read32(part);
    if (type != 2 && type != 7) continue;
    uint64_t offset = read64(part + 8), address = read64(part + 16);
    uint64_t filesz = read64(part + 32), memsz = read64(part + 40);
    unsigned int matches = 0;
    for (size_t load = 0; load < phnum; ++load) {
      const unsigned char *segment = file.data + phoff + load * 56;
      if (read32(segment) != 1) continue;
      uint64_t file_start = read64(segment + 8), start = read64(segment + 16);
      if (address >= start && offset >= file_start &&
          address - start == offset - file_start &&
          inside(address - start, memsz, read64(segment + 40)) &&
          inside(offset - file_start, filesz, read64(segment + 32))) ++matches;
    }
    if (matches != 1) return PS_IMAGE;
  }
  state->phdr = request->image_bias + phoff;
  state->entry = request->image_bias + request->crt_entry_offset;
  state->phnum = phnum;
  return PS_OK;
}

static int retained_aux(uint64_t type)
{
  return type == 1 || type == 2 || (type >= 3 && type <= 14) ||
    (type >= 16 && type <= 23) || (type >= 26 && type <= 30) ||
    (type >= 32 && type <= 37) || (type >= 40 && type <= 47) || type == 51;
}

static enum ps_status decode(const struct ps_request *request, struct decoded *state)
{
  if (!request || !request->guest_stack.data || !request->image_file.data ||
      !request->tool_name.data) return PS_ARGUMENT;
  struct ps_view guest = request->guest_stack;
  uintptr_t base = (uintptr_t) guest.data;
  if (!range(base, guest.size) || !range((uintptr_t) request->image_file.data,
      request->image_file.size) || !range((uintptr_t) request->tool_name.data,
      request->tool_name.size)) return PS_OVERFLOW;
  if (base % 16 || guest.size < 8 || guest.size > PS_MAX_STACK ||
      !request->tool_name.size || request->tool_name.size > 4096 ||
      request->call_stack_reserve < 65536) return PS_ARGUMENT;
  size_t length;
  if (!string_size(request->tool_name, (uintptr_t) request->tool_name.data, &length) ||
      length != request->tool_name.size) return PS_ARGUMENT;
  enum ps_status status = image(request, state);
  if (status != PS_OK) return status;
  size_t argc = read64(guest.data);
  size_t string_budget = PS_MAX_STACK;
  if (argc > PS_MAX_VECTOR || !inside(8, (argc + 1) * 8, guest.size)) return PS_STACK;
  for (size_t index = 0; index < argc; ++index)
    if (!checked_string(guest, read64(guest.data + 8 + index * 8), &length,
                        &string_budget)) return PS_STACK;
  if (read64(guest.data + 8 + argc * 8)) return PS_STACK;
  size_t offset = (argc + 2) * 8;
  state->argc = argc;
  state->env_offset = offset;
  state->envc = 0;
  size_t data_size = request->tool_name.size;
  for (;;) {
    if (!inside(offset, 8, guest.size)) return PS_STACK;
    uintptr_t pointer = read64(guest.data + offset);
    offset += 8;
    if (!pointer) break;
    if (++state->envc > PS_MAX_VECTOR ||
        !checked_string(guest, pointer, &length, &string_budget))
      return PS_STACK;
    if (!add(&data_size, length)) return PS_OVERFLOW;
  }
  state->aux_offset = offset;
  state->auxc = 0;
  uint64_t required = 0;
  for (;;) {
    if (!inside(offset, 16, guest.size)) return PS_AUX;
    uint64_t type = read64(guest.data + offset);
    uintptr_t value = read64(guest.data + offset + 8);
    if (!type) { if (value) return PS_AUX; break; }
    if (state->auxc == PS_MAX_AUX || type == PS_AUX_HANDOFF) return PS_UNSUPPORTED;
    for (size_t prior = 0; prior < state->auxc; ++prior)
      if (read64(guest.data + state->aux_offset + prior * 16) == type) return PS_AUX;
    if (type == 15 || type == 24 || type == 31) {
      if (!checked_string(guest, value, &length, &string_budget)) return PS_AUX;
      if (type != 31 && !add(&data_size, length)) return PS_OVERFLOW;
    } else if (type == 25) {
      if (value < base || !inside(value - base, 16, guest.size)) return PS_AUX;
      state->random = value;
      if (!add(&data_size, 16)) return PS_OVERFLOW;
    } else if (!retained_aux(type)) return PS_UNSUPPORTED;
    if (type == 4 && value != 56) return PS_UNSUPPORTED;
    if (type == 5 && !value) return PS_AUX;
    if (type == 6 && value != 4096) return PS_UNSUPPORTED;
    if ((type == 3 || type == 7 || type == 9) && !value) return PS_UNSUPPORTED;
    if (type < 64) required |= UINT64_C(1) << type;
    ++state->auxc;
    offset += 16;
  }
  uint64_t mandatory = (UINT64_C(1) << 3) | (UINT64_C(1) << 4) |
    (UINT64_C(1) << 5) | (UINT64_C(1) << 6) | (UINT64_C(1) << 7) |
    (UINT64_C(1) << 9) | (UINT64_C(1) << 23) | (UINT64_C(1) << 25) |
    (UINT64_C(1) << 31);
  if ((required & mandatory) != mandatory) return PS_AUX;
  uintptr_t data_begin = base + offset + 16;
  for (size_t index = 0; index < argc; ++index)
    if (read64(guest.data + 8 + index * 8) < data_begin) return PS_STACK;
  for (size_t index = 0; index < state->envc; ++index)
    if (read64(guest.data + state->env_offset + index * 8) < data_begin) return PS_STACK;
  for (size_t index = 0; index < state->auxc; ++index) {
    const unsigned char *pair = guest.data + state->aux_offset + index * 16;
    uint64_t type = read64(pair);
    if ((type == 15 || type == 24 || type == 25 || type == 31) &&
        read64(pair + 8) < data_begin) return PS_AUX;
  }
  size_t payload = (state->envc + 4) * 8 + (state->auxc + 2) * 16;
  if (!add(&payload, data_size) || !add(&payload, 7 + sizeof(struct ps_handoff) + 15))
    return PS_OVERFLOW;
  state->payload = payload & ~(size_t) 15;
  return PS_OK;
}

static int conflicts(const struct ps_request *request, uintptr_t base, size_t size)
{
  return overlap(base, size, (uintptr_t) request, sizeof *request) ||
    overlap(base, size, (uintptr_t) request->guest_stack.data, request->guest_stack.size) ||
    overlap(base, size, (uintptr_t) request->image_file.data, request->image_file.size) ||
    overlap(base, size, request->image_bias, request->image_mapping_size) ||
    overlap(base, size, (uintptr_t) request->tool_name.data, request->tool_name.size);
}

enum ps_status ps_measure(const struct ps_request *request, size_t *required)
{
  struct decoded state;
  enum ps_status status = decode(request, &state);
  if (status != PS_OK) return status;
  if (!required || !range((uintptr_t) required, sizeof *required)) return PS_ARGUMENT;
  if (conflicts(request, (uintptr_t) required, sizeof *required)) return PS_OVERLAP;
  size_t size = state.payload;
  if (!add(&size, request->call_stack_reserve) || !add(&size, 15)) return PS_OVERFLOW;
  *required = size & ~(size_t) 15;
  return PS_OK;
}

enum ps_status ps_build(const struct ps_request *request, void *storage,
                       size_t size, struct ps_launch *launch)
{
  struct decoded state;
  enum ps_status status = decode(request, &state);
  if (status != PS_OK) return status;
  uintptr_t base = (uintptr_t) storage;
  if (!storage || !launch || base % 16 || size % 16 || !range(base, size) ||
      !range((uintptr_t) launch, sizeof *launch)) return PS_ARGUMENT;
  if (state.payload > size || request->call_stack_reserve > size - state.payload)
    return PS_SPACE;
  if (conflicts(request, base, size) || conflicts(request, (uintptr_t) launch, sizeof *launch) ||
      overlap(base, size, (uintptr_t) launch, sizeof *launch)) return PS_OVERLAP;
  unsigned char *stack = (unsigned char *) storage + size - state.payload;
  unsigned char *data = stack + (state.envc + 4) * 8 + (state.auxc + 2) * 16;
  uintptr_t name = (uintptr_t) data;
  copy(data, request->tool_name.data, request->tool_name.size);
  data += request->tool_name.size;
  write64(stack, 1);
  write64(stack + 8, name);
  write64(stack + 16, 0);
  size_t slot = 24;
  for (size_t index = 0; index < state.envc; ++index) {
    uintptr_t pointer = read64(request->guest_stack.data + state.env_offset + index * 8);
    size_t length = 0;
    string_size(request->guest_stack, pointer, &length);
    write64(stack + slot, (uintptr_t) data);
    copy(data, (const unsigned char *) pointer, length);
    data += length;
    slot += 8;
  }
  write64(stack + slot, 0);
  slot += 8;
  for (size_t index = 0; index < state.auxc; ++index) {
    const unsigned char *pair = request->guest_stack.data + state.aux_offset + index * 16;
    uint64_t type = read64(pair);
    uintptr_t value = read64(pair + 8);
    if (type == 3) value = state.phdr;
    else if (type == 4) value = 56;
    else if (type == 5) value = state.phnum;
    else if (type == 7) value = 0;
    else if (type == 9) value = state.entry;
    else if (type == 31) value = name;
    else if (type == 33) value = 0;
    else if (type == 15 || type == 24 || type == 25) {
      size_t length = 16;
      if (type != 25) string_size(request->guest_stack, value, &length);
      copy(data, (const unsigned char *) value, length);
      value = (uintptr_t) data;
      data += length;
    }
    write64(stack + slot, type);
    write64(stack + slot + 8, value);
    slot += 16;
  }
  struct ps_handoff *handoff = (void *) (((uintptr_t) data + 7) & ~(uintptr_t) 7);
  handoff->version = PS_ABI_VERSION;
  handoff->size = sizeof *handoff;
  handoff->original_sp = (uintptr_t) request->guest_stack.data;
  handoff->original_stack_end = handoff->original_sp + request->guest_stack.size;
  handoff->original_argv = handoff->original_sp + 8;
  handoff->original_argc = state.argc;
  handoff->original_envp = handoff->original_sp + state.env_offset;
  handoff->original_envc = state.envc;
  handoff->original_auxv = handoff->original_sp + state.aux_offset;
  handoff->original_auxc = state.auxc;
  handoff->original_random = state.random;
  handoff->private_stack_begin = base;
  handoff->private_stack_end = base + size;
  write64(stack + slot, PS_AUX_HANDOFF);
  write64(stack + slot + 8, (uintptr_t) handoff);
  write64(stack + slot + 16, 0);
  write64(stack + slot + 24, 0);
  launch->crt_entry = state.entry;
  launch->crt_sp = (uintptr_t) stack;
  launch->crt_rdx = 0;
  launch->handoff = handoff;
  return PS_OK;
}
