#define _GNU_SOURCE
#include "mapper.h"
#include <asm/unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <sys/mman.h>
#include <sys/vfs.h>

enum ma_status pm_read_maps(uintptr_t rsp, struct ma_owner *maps, size_t *used) {
  if (maps == NULL || used == NULL || maps->maps_storage != NULL || maps->runtime_file.data != NULL)
    return MA_ARGUMENT;
  *maps = (struct ma_owner) {0};
  long result = ma_raw(__NR_mmap, 0, MA_MAPS_LIMIT, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if ((unsigned long)result >= (unsigned long)-4095) {
    maps->status = MA_IO; maps->primary_error = result; return MA_IO;
  }
  maps->maps_storage = (void *)result; maps->maps_size = MA_MAPS_LIMIT;
  int descriptor = -1;
  result = ma_raw(__NR_openat, AT_FDCWD, (long)"/proc/self/maps", O_RDONLY | O_CLOEXEC | O_NOFOLLOW, 0, 0, 0);
  if (result < 0) goto io_failure;
  descriptor = (int)result;
  struct statfs filesystem;
  result = ma_raw(__NR_fstatfs, descriptor, (long)&filesystem, 0, 0, 0, 0);
  if (result < 0) goto io_failure;
  if (filesystem.f_type != 0x9fa0) { maps->status = MA_PROCFS; goto cleanup; }
  size_t length = 0;
  int eof = 0;
  for (unsigned int attempt = 0; attempt < MA_IO_LIMIT; ++attempt) {
    unsigned char extra;
    size_t available = length < MA_MAPS_LIMIT ? MA_MAPS_LIMIT - length : 1;
    void *buffer = length < MA_MAPS_LIMIT ? (unsigned char *)maps->maps_storage + length : &extra;
    result = ma_raw(__NR_read, descriptor, (long)buffer, (long)available, 0, 0, 0);
    if (result == -EINTR) continue;
    if (result < 0) goto io_failure;
    if (result == 0) { eof = 1; break; }
    if ((size_t)result > available || length == MA_MAPS_LIMIT) { maps->status = MA_LIMIT; goto cleanup; }
    length += (size_t)result;
  }
  if (!eof) { maps->status = MA_LIMIT; goto cleanup; }
  maps->status = ma_stack_extent(maps->maps_storage, length, rsp, &maps->original_stack);
  if (maps->status != MA_OK) goto cleanup;
  result = ma_raw(__NR_close, descriptor, 0, 0, 0, 0, 0);
  descriptor = -1;
  if (result < 0) { maps->status = MA_CLEANUP; maps->primary_error = result; goto cleanup; }
  *used = length;
  return MA_OK;
io_failure:
  maps->status = MA_IO; maps->primary_error = result;
cleanup:
  if (descriptor >= 0) {
    long closed = ma_raw(__NR_close, descriptor, 0, 0, 0, 0, 0);
    if (closed < 0 && maps->cleanup_error == 0) maps->cleanup_error = closed;
  }
  (void)ma_release(maps);
  return maps->status;
}

static uint64_t word(const unsigned char *bytes) {
  uint64_t value = 0;
  for (size_t index = 0; index < 8; ++index) value |= (uint64_t)bytes[index] << (index * 8);
  return value;
}

static enum pm_status discover(struct ps_view stack, int *fd, uintptr_t *at_base,
                               const char *prefix, size_t prefix_size) {
  if (stack.data == NULL || fd == NULL || at_base == NULL || stack.size < 24 ||
      stack.size > PS_MAX_STACK || (uintptr_t)stack.data % 16 ||
      stack.size > UINTPTR_MAX - (uintptr_t)stack.data) return PM_ARGUMENT;
  size_t argc = (size_t)word(stack.data);
  if (argc > PS_MAX_VECTOR || argc > (stack.size - 16) / 8) return PM_DISCOVERY;
  size_t cursor = (argc + 1) * 8;
  if (word(stack.data + cursor) != 0) return PM_DISCOVERY;
  cursor += 8;
  size_t budget = PS_MAX_STACK;
  int found = -1;
  size_t envc = 0;
  while (cursor <= stack.size - 8 && word(stack.data + cursor) != 0) {
    if (++envc > PS_MAX_VECTOR) return PM_DISCOVERY;
    uintptr_t address = word(stack.data + cursor);
    if (address < (uintptr_t)stack.data || address - (uintptr_t)stack.data >= stack.size)
      return PM_DISCOVERY;
    size_t offset = address - (uintptr_t)stack.data, length = 0;
    while (length < stack.size - offset && stack.data[offset + length] != 0) {
      if (budget == 0) return PM_DISCOVERY;
      --budget; ++length;
    }
    if (length == stack.size - offset) return PM_DISCOVERY;
    const unsigned char *text = stack.data + offset;
    int match = length >= prefix_size - 1;
    for (size_t index = 0; match && index < prefix_size - 1; ++index)
      if (text[index] != (unsigned char)prefix[index]) match = 0;
    if (match) {
      if (found >= 0 || length == prefix_size - 1 || text[prefix_size - 1] == '0')
        return PM_DISCOVERY;
      unsigned int value = 0;
      for (size_t index = prefix_size - 1; index < length; ++index) {
        unsigned int digit = text[index];
        if (digit < '0' || digit > '9' || value > ((unsigned int)INT_MAX - (digit - '0')) / 10)
          return PM_DISCOVERY;
        value = value * 10 + digit - '0';
      }
      if (value < 3) return PM_DISCOVERY;
      found = (int)value;
    }
    cursor += 8;
  }
  if (cursor > stack.size - 8 || found < 0) return PM_DISCOVERY;
  cursor += 8;
  uintptr_t base = 0;
  unsigned int bases = 0;
  for (size_t count = 0; count < PS_MAX_AUX && cursor <= stack.size && stack.size - cursor >= 16; ++count) {
    uint64_t tag = word(stack.data + cursor), value = word(stack.data + cursor + 8);
    if (tag == 0) {
      if (bases != 1 || base == 0 || base % PM_PAGE) return PM_DISCOVERY;
      *fd = found; *at_base = base; return PM_OK;
    }
    if (tag == 7) { ++bases; base = value; }
    cursor += 16;
  }
  return PM_DISCOVERY;
}

enum pm_status pm_discover(struct ps_view stack, int *fd, uintptr_t *at_base) {
  return discover(stack, fd, at_base, PM_IMAGE_ENV "=", sizeof(PM_IMAGE_ENV "="));
}

enum pm_status pm_discover_original(struct ps_view stack, int *fd, uintptr_t *at_base) {
  return discover(stack, fd, at_base, PM_ORIGINAL_ENV "=", sizeof(PM_ORIGINAL_ENV "="));
}

enum pm_status pm_discover_restoration(struct ps_view stack, int *fd, uintptr_t *at_base) {
  return discover(stack, fd, at_base, PM_BINDING_ENV "=", sizeof(PM_BINDING_ENV "="));
}
