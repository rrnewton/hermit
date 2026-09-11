#include "mapper.h"
#include <asm/unistd.h>
#include <sys/stat.h>

static uint64_t integer(const unsigned char **cursor, unsigned int base) {
  uint64_t value = 0;
  while (**cursor != ' ' && **cursor != '-' && **cursor != ':' && **cursor != '\n') {
    unsigned int digit = *(*cursor)++;
    if (digit >= 'a' && digit <= 'f') digit -= 'a' - 10;
    else if (digit >= 'A' && digit <= 'F') digit -= 'A' - 10;
    else digit -= '0';
    value = value * base + digit;
  }
  return value;
}

static void spaces(const unsigned char **cursor) {
  while (**cursor == ' ') ++*cursor;
}

enum pm_status pm_check_binding(const struct pe_entry *entry, int fd,
                               struct ps_view file, uintptr_t crt, uintptr_t at_base,
                               struct ps_view maps) {
  if (entry == NULL || at_base == 0 || at_base % PM_PAGE) return PM_ARGUMENT;
  struct ps_view checked;
  if (ma_stack_extent(maps.data, maps.size, entry->rsp, &checked) != MA_OK) return PM_BINDING;
  struct pm_plan plan;
  if (pm_validate(file, crt, &plan) != PM_OK || at_base > UINTPTR_MAX - plan.span ||
      entry->entry_pc != at_base + plan.kernel_entry) return PM_BINDING;
  struct stat metadata;
  long result = ma_raw(__NR_fstat, fd, (long)&metadata, 0, 0, 0, 0);
  if (result != 0) return PM_BINDING;
  uint64_t major = ((metadata.st_dev >> 8) & 0xfff) | ((metadata.st_dev >> 32) & 0xfffff000);
  uint64_t minor = (metadata.st_dev & 0xff) | ((metadata.st_dev >> 12) & 0xffffff00);
  for (size_t index = 0; index < plan.count; ++index) {
    const struct pm_segment *load = &plan.loads[index];
    if (load->filesz == 0) continue;
    uintptr_t needed = at_base + load->page_begin;
    uintptr_t end = at_base + ((load->address + load->filesz + PM_PAGE - 1) & ~(uintptr_t)(PM_PAGE - 1));
    uint64_t file_offset = load->offset & ~(uint64_t)(PM_PAGE - 1);
    const unsigned char *cursor = maps.data, *stop = maps.data + maps.size;
    while (cursor < stop && needed < end) {
      uint64_t begin = integer(&cursor, 16); ++cursor;
      uint64_t finish = integer(&cursor, 16); spaces(&cursor);
      int readable = cursor[0] == 'r';
      cursor += 4; spaces(&cursor);
      uint64_t offset = integer(&cursor, 16); spaces(&cursor);
      uint64_t dev_major = integer(&cursor, 16); ++cursor;
      uint64_t dev_minor = integer(&cursor, 16); spaces(&cursor);
      uint64_t inode = integer(&cursor, 10);
      while (*cursor != '\n') ++cursor;
      ++cursor;
      if (begin <= needed && needed < finish) {
        if (!readable || inode != metadata.st_ino || dev_major != major || dev_minor != minor ||
            offset > UINT64_MAX - (needed - begin) || offset + needed - begin != file_offset)
          return PM_BINDING;
        uintptr_t next = finish < end ? finish : end;
        file_offset += next - needed; needed = next;
      }
    }
    if (needed != end) return PM_BINDING;
  }
  return PM_OK;
}
