#include "acquire.h"
#include <elf.h>

static uint64_t get(const unsigned char *bytes, size_t count) {
  uint64_t result = 0;
  for (size_t index = 0; index < count; ++index)
    result |= (uint64_t)bytes[index] << (8 * index);
  return result;
}

static int inside(uint64_t offset, uint64_t size, size_t total) {
  return offset <= total && size <= total - offset;
}

static int name_matches(const unsigned char *name, size_t available) {
  const char expected[] = MA_CRT_SYMBOL;
  if (available < sizeof expected) return 0;
  for (size_t index = 0; index < sizeof expected; ++index)
    if (name[index] != (unsigned char)expected[index]) return 0;
  return 1;
}

enum ma_status ma_crt_symbol(struct ps_view image, uintptr_t *offset) {
  if (image.data == NULL || offset == NULL || image.size < 64 ||
      image.size > MA_IMAGE_LIMIT) return MA_ARGUMENT;
  const unsigned char *bytes = image.data;
  if (get(bytes, 4) != 0x464c457f || bytes[4] != ELFCLASS64 ||
      bytes[5] != ELFDATA2LSB || bytes[6] != EV_CURRENT ||
      get(bytes + 16, 2) != ET_DYN || get(bytes + 18, 2) != EM_X86_64 ||
      get(bytes + 20, 4) != EV_CURRENT || get(bytes + 52, 2) != 64 ||
      get(bytes + 54, 2) != 56 || get(bytes + 58, 2) != 64) return MA_IMAGE;
  uint64_t phoff = get(bytes + 32, 8), shoff = get(bytes + 40, 8);
  uint64_t phnum = get(bytes + 56, 2), shnum = get(bytes + 60, 2);
  if (phnum == 0 || phnum > 128 || shnum == 0 || shnum > 4096 ||
      !inside(phoff, phnum * 56, image.size) ||
      !inside(shoff, shnum * 64, image.size)) return MA_IMAGE;
  uint64_t value = 0, length = 0, target_section = 0;
  unsigned int matches = 0;
  size_t symbols_scanned = 0;
  for (uint64_t index = 0; index < shnum; ++index) {
    const unsigned char *section = bytes + shoff + index * 64;
    if (get(section + 4, 4) != SHT_SYMTAB) continue;
    uint64_t start = get(section + 24, 8), size = get(section + 32, 8);
    uint64_t link = get(section + 40, 4);
    if (get(section + 56, 8) != 24 || size % 24 != 0 || link >= shnum ||
        !inside(start, size, image.size) || size / 24 > 1000000 - symbols_scanned)
      return MA_IMAGE;
    symbols_scanned += size / 24;
    const unsigned char *strings = bytes + shoff + link * 64;
    uint64_t str_start = get(strings + 24, 8), str_size = get(strings + 32, 8);
    if (get(strings + 4, 4) != SHT_STRTAB ||
        !inside(str_start, str_size, image.size)) return MA_IMAGE;
    for (uint64_t position = 0; position < size; position += 24) {
      const unsigned char *symbol = bytes + start + position;
      uint64_t name = get(symbol, 4);
      if (name >= str_size) return MA_IMAGE;
      if (!name_matches(bytes + str_start + name, str_size - name)) continue;
      unsigned int binding = ELF64_ST_BIND(symbol[4]);
      if (++matches != 1 || ELF64_ST_TYPE(symbol[4]) != STT_FUNC ||
          !((binding == STB_GLOBAL && symbol[5] == STV_HIDDEN) ||
            (binding == STB_LOCAL && (symbol[5] == STV_DEFAULT || symbol[5] == STV_HIDDEN))))
        return MA_CRT;
      target_section = get(symbol + 6, 2);
      value = get(symbol + 8, 8); length = get(symbol + 16, 8);
      if (target_section == 0 || target_section >= shnum || value == 0 ||
          length == 0 || length > UINT64_MAX - value) return MA_CRT;
    }
  }
  if (matches != 1) return MA_CRT;
  const unsigned char *target = bytes + shoff + target_section * 64;
  uint64_t sec_addr = get(target + 16, 8), sec_off = get(target + 24, 8);
  uint64_t sec_size = get(target + 32, 8);
  if (get(target + 4, 4) != SHT_PROGBITS ||
      (get(target + 8, 8) & (SHF_ALLOC | SHF_EXECINSTR | SHF_WRITE)) !=
        (SHF_ALLOC | SHF_EXECINSTR) || !inside(sec_off, sec_size, image.size) ||
      value < sec_addr || value - sec_addr > sec_size ||
      length > sec_size - (value - sec_addr)) return MA_CRT;
  unsigned int covering = 0;
  for (uint64_t index = 0; index < phnum; ++index) {
    const unsigned char *header = bytes + phoff + index * 56;
    uint64_t type = get(header, 4);
    if (type == PT_INTERP) return MA_IMAGE;
    if (type != PT_LOAD) continue;
    uint64_t start = get(header + 16, 8), file = get(header + 8, 8);
    uint64_t filesz = get(header + 32, 8), memsz = get(header + 40, 8);
    if (filesz > memsz || memsz > UINT64_MAX - start ||
        !inside(file, filesz, image.size)) return MA_IMAGE;
    if (start < value + length && start + memsz > value) {
      if (++covering != 1 || get(header + 4, 4) != (PF_R | PF_X) ||
          value < start || value - start > filesz ||
          length > filesz - (value - start) ||
          file + value - start != sec_off + value - sec_addr) return MA_CRT;
    }
  }
  if (covering != 1) return MA_CRT;
  *offset = value;
  return MA_OK;
}
