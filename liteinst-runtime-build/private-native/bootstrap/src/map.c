#define _GNU_SOURCE
#include "mapper.h"
#include <asm/unistd.h>
#include <elf.h>
#include <sys/mman.h>

static uint64_t get(const unsigned char *source, size_t count) {
  uint64_t result = 0;
  for (size_t index = 0; index < count; ++index)
    result |= (uint64_t)source[index] << (index * 8);
  return result;
}

static int contains(uint64_t start, uint64_t size, uint64_t at, uint64_t length) {
  return at >= start && at - start <= size && length <= size - (at - start);
}

enum pm_status pm_validate(struct ps_view file, uintptr_t crt, struct pm_plan *plan) {
  uintptr_t symbol;
  if (plan == NULL || ma_crt_symbol(file, &symbol) != MA_OK || symbol != crt)
    return PM_FORMAT;
  const unsigned char *bytes = file.data;
  size_t phoff = (size_t)get(bytes + 32, 8), phnum = (size_t)get(bytes + 56, 2);
  if (phoff < 64 || phoff % 8 != 0) return PM_FORMAT;
  struct pm_plan result = {0};
  result.alignment = PM_PAGE;
  result.crt_entry = crt;
  result.kernel_entry = get(bytes + 24, 8);
  unsigned int header_load = 0, entry_load = 0, dynamic = 0, tls = 0, relro = 0;
  for (size_t index = 0; index < phnum; ++index) {
    const unsigned char *ph = bytes + phoff + index * 56;
    uint64_t type = get(ph, 4), flags = get(ph + 4, 4);
    uint64_t off = get(ph + 8, 8), addr = get(ph + 16, 8);
    uint64_t filesz = get(ph + 32, 8), memsz = get(ph + 40, 8), align = get(ph + 48, 8);
    if (type == PT_INTERP || (type == PT_GNU_STACK && (flags & PF_X))) return PM_UNSUPPORTED;
    if (type != PT_LOAD && type != PT_TLS && type != PT_DYNAMIC && type != PT_GNU_RELRO) {
      if (type != PT_NULL && type != PT_PHDR && type != PT_NOTE && type != PT_GNU_STACK &&
          type != PT_GNU_EH_FRAME && type != PT_GNU_PROPERTY) return PM_UNSUPPORTED;
      continue;
    }
    if (filesz > memsz || !contains(0, file.size, off, filesz) ||
        !contains(0, PM_MAX_SPAN, addr, memsz) ||
        (align > 1 && ((align & (align - 1)) || addr % align != off % align)))
      return PM_FORMAT;
    if (type == PT_LOAD) {
      if (memsz == 0 || !(flags & PF_R) || (flags & ~7u) ||
          ((flags & (PF_W | PF_X)) == (PF_W | PF_X)) ||
          addr % PM_PAGE != off % PM_PAGE || align > PM_MAX_ALIGN ||
          (filesz == 0 && addr % PM_PAGE != 0)) return PM_UNSUPPORTED;
      size_t begin = addr & ~(size_t)(PM_PAGE - 1);
      size_t end = (addr + memsz + PM_PAGE - 1) & ~(size_t)(PM_PAGE - 1);
      for (size_t previous = 0; previous < result.count; ++previous)
        if (begin < result.loads[previous].page_end && end > result.loads[previous].page_begin)
          return PM_OVERLAP;
      result.loads[result.count++] = (struct pm_segment) {addr, off, filesz, memsz, begin, end, (unsigned int)flags};
      if (end > result.span) result.span = end;
      if (align > result.alignment) result.alignment = align;
      if (addr == 0 && off == 0 && contains(0, filesz, 0, phoff + phnum * 56)) ++header_load;
      if ((flags & PF_X) && result.kernel_entry != 0 && contains(addr, filesz, result.kernel_entry, 1)) ++entry_load;
    } else if (type == PT_DYNAMIC) {
      if (++dynamic != 1 || filesz == 0 || filesz % 16 || off % 8 || addr % 8) return PM_FORMAT;
      int terminated = 0;
      for (size_t cursor = 0; cursor < filesz; cursor += 16) {
        uint64_t tag = get(bytes + off + cursor, 8), value = get(bytes + off + cursor + 8, 8);
        if (tag == DT_NULL) { terminated = 1; break; }
        if (tag == DT_NEEDED || tag == DT_TEXTREL || (tag == DT_FLAGS && (value & DF_TEXTREL)))
          return PM_UNSUPPORTED;
      }
      if (!terminated) return PM_FORMAT;
    } else if (type == PT_TLS) {
      if (++tls != 1 || align > PM_MAX_ALIGN) return PM_FORMAT;
    } else {
      if (++relro != 1 || memsz == 0) return PM_FORMAT;
      result.relro_begin = addr & ~(size_t)(PM_PAGE - 1);
      result.relro_end = (addr + memsz) & ~(size_t)(PM_PAGE - 1);
    }
  }
  if (header_load != 1 || entry_load != 1 || dynamic != 1 || tls != 1) return PM_FORMAT;
  for (size_t index = 0; index < phnum; ++index) {
    const unsigned char *ph = bytes + phoff + index * 56;
    uint64_t type = get(ph, 4);
    if (type != PT_DYNAMIC && type != PT_TLS && type != PT_GNU_RELRO) continue;
    uint64_t off = get(ph + 8, 8), addr = get(ph + 16, 8);
    uint64_t filesz = get(ph + 32, 8), memsz = get(ph + 40, 8);
    unsigned int covering = 0;
    for (size_t load = 0; load < result.count; ++load) {
      const struct pm_segment *segment = &result.loads[load];
      if (segment->address < addr + memsz && segment->address + segment->memsz > addr) {
        if (!contains(segment->address, segment->memsz, addr, memsz) ||
            !contains(segment->offset, segment->filesz, off, filesz) ||
            addr - segment->address != off - segment->offset ||
            (type == PT_GNU_RELRO && segment->flags != (PF_R | PF_W))) return PM_FORMAT;
        ++covering;
      }
    }
    if (covering != 1) return PM_FORMAT;
  }
  *plan = result;
  return PM_OK;
}

enum pm_status pm_unmap(struct pm_image *image) {
  if (image == NULL) return PM_ARGUMENT;
  if (image->reservation == NULL) return PM_OK;
  long result = ma_raw(__NR_munmap, (long)image->reservation, (long)image->reservation_size, 0, 0, 0, 0);
  if (result != 0) {
    if (image->cleanup_error == 0) image->cleanup_error = result;
    return PM_CLEANUP;
  }
  image->reservation = NULL; image->reservation_size = 0; image->bias = 0; image->span = 0;
  return PM_OK;
}

enum pm_status pm_map(struct ps_view file, uintptr_t crt, struct pm_image *image) {
  if (image == NULL || image->reservation != NULL) return PM_ARGUMENT;
  struct pm_plan plan;
  enum pm_status status = pm_validate(file, crt, &plan);
  if (status != PM_OK) return status;
  *image = (struct pm_image) {0};
  size_t size = plan.span + plan.alignment - PM_PAGE;
  long allocated = ma_raw(__NR_mmap, 0, (long)size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if ((unsigned long)allocated >= (unsigned long)-4095) {
    image->status = PM_IO; image->primary_error = allocated; return PM_IO;
  }
  image->reservation = (void *)allocated; image->reservation_size = size;
  image->bias = ((uintptr_t)allocated + plan.alignment - 1) & ~(uintptr_t)(plan.alignment - 1);
  image->span = plan.span; image->relro_begin = plan.relro_begin; image->relro_end = plan.relro_end;
  for (size_t index = 0; index < plan.count; ++index) {
    const struct pm_segment *load = &plan.loads[index];
    unsigned char *destination = (void *)(image->bias + load->page_begin);
    size_t length = load->page_end - load->page_begin;
    long changed = ma_raw(__NR_mprotect, (long)destination, (long)length, PROT_READ | PROT_WRITE, 0, 0, 0);
    if (changed != 0) { image->primary_error = changed; goto failure; }
    if (load->filesz != 0) {
      size_t off = load->offset & ~(size_t)(PM_PAGE - 1);
      size_t file_end = (load->address + load->filesz + PM_PAGE - 1) & ~(size_t)(PM_PAGE - 1);
      size_t copied = file_end - load->page_begin;
      if (copied > file.size - off) copied = file.size - off;
      for (size_t position = 0; position < copied; ++position) destination[position] = file.data[off + position];
    }
    unsigned char *bss = (void *)(image->bias + load->address + load->filesz);
    for (size_t position = 0; position < load->memsz - load->filesz; ++position) bss[position] = 0;
    int protection = PROT_READ;
    if (load->flags & PF_W) protection |= PROT_WRITE;
    if (load->flags & PF_X) protection |= PROT_EXEC;
    changed = ma_raw(__NR_mprotect, (long)destination, (long)length, protection, 0, 0, 0);
    if (changed != 0) { image->primary_error = changed; goto failure; }
  }
  image->status = PM_OK;
  return PM_OK;
failure:
  image->status = PM_IO;
  (void)pm_unmap(image);
  return PM_IO;
}
