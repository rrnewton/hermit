#define _GNU_SOURCE
#include "acquire.h"
#include <asm/unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/vfs.h>

_Static_assert(sizeof(long) == 8, "x86-64 syscall words");
_Static_assert(sizeof(struct stat) == 144, "x86-64 kernel stat size");
_Static_assert(offsetof(struct stat, st_mode) == 24, "x86-64 kernel stat mode");
_Static_assert(offsetof(struct stat, st_size) == 48, "x86-64 kernel stat size field");
_Static_assert(sizeof(struct statfs) == 120, "x86-64 kernel statfs size");
_Static_assert(offsetof(struct statfs, f_type) == 0, "x86-64 kernel filesystem type");

static void cleanup_error(struct ma_owner *owner, long error) {
  if (error < 0 && owner->cleanup_error == 0) owner->cleanup_error = error;
}

enum ma_status ma_release(struct ma_owner *owner) {
  if (owner == NULL) return MA_ARGUMENT;
  int failed = 0;
  if (owner->maps_storage != NULL) {
    long result = ma_raw(__NR_munmap, (long)owner->maps_storage,
                         (long)owner->maps_size, 0, 0, 0, 0);
    if (result == 0) { owner->maps_storage = NULL; owner->maps_size = 0; }
    else { cleanup_error(owner, result); failed = 1; }
  }
  if (owner->runtime_file.data != NULL) {
    long result = ma_raw(__NR_munmap, (long)owner->runtime_file.data,
                         (long)owner->runtime_file.size, 0, 0, 0, 0);
    if (result == 0) owner->runtime_file = (struct ps_view) {0};
    else { cleanup_error(owner, result); failed = 1; }
  }
  if (failed) return MA_CLEANUP;
  owner->original_stack = (struct ps_view) {0};
  owner->crt_entry_offset = 0;
  return MA_OK;
}

static enum ma_status fail(struct ma_owner *owner, enum ma_status status,
                           long error, int proc_fd) {
  owner->status = status;
  owner->primary_error = error;
  if (proc_fd >= 0)
    cleanup_error(owner, ma_raw(__NR_close, proc_fd, 0, 0, 0, 0, 0));
  (void)ma_release(owner);
  return status;
}

enum ma_status ma_acquire(uintptr_t saved_rsp, int sealed_runtime_fd,
                         struct ma_owner *owner) {
  if (owner == NULL || owner->runtime_file.data != NULL ||
      owner->maps_storage != NULL || saved_rsp == 0 || sealed_runtime_fd < 0)
    return MA_ARGUMENT;
  *owner = (struct ma_owner) {0};
  long seals = ma_raw(__NR_fcntl, sealed_runtime_fd, F_GET_SEALS, 0, 0, 0, 0);
  const int required = F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE;
  if (seals < 0 || (seals & required) != required)
    return fail(owner, MA_SEALS, seals < 0 ? seals : 0, -1);
  struct stat metadata;
  long result = ma_raw(__NR_fstat, sealed_runtime_fd, (long)&metadata, 0, 0, 0, 0);
  if (result < 0) return fail(owner, MA_IO, result, -1);
  if (!S_ISREG(metadata.st_mode) || metadata.st_size < 64 ||
      (uint64_t)metadata.st_size > MA_IMAGE_LIMIT) return fail(owner, MA_IMAGE, 0, -1);
  size_t image_size = (size_t)metadata.st_size;
  result = ma_raw(__NR_mmap, 0, (long)image_size, PROT_READ, MAP_PRIVATE,
                  sealed_runtime_fd, 0);
  if ((unsigned long)result >= (unsigned long)-4095)
    return fail(owner, MA_IO, result, -1);
  owner->runtime_file = (struct ps_view) {(const unsigned char *)result, image_size};
  enum ma_status status = ma_crt_symbol(owner->runtime_file, &owner->crt_entry_offset);
  if (status != MA_OK) return fail(owner, status, 0, -1);
  result = ma_raw(__NR_mmap, 0, MA_MAPS_LIMIT, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if ((unsigned long)result >= (unsigned long)-4095)
    return fail(owner, MA_IO, result, -1);
  owner->maps_storage = (void *)result;
  owner->maps_size = MA_MAPS_LIMIT;
  result = ma_raw(__NR_openat, AT_FDCWD, (long)"/proc/self/maps",
                  O_RDONLY | O_CLOEXEC | O_NOFOLLOW, 0, 0, 0);
  if (result < 0) return fail(owner, MA_IO, result, -1);
  int proc_fd = (int)result;
  struct statfs filesystem;
  result = ma_raw(__NR_fstatfs, proc_fd, (long)&filesystem, 0, 0, 0, 0);
  if (result < 0) return fail(owner, MA_IO, result, proc_fd);
  if (filesystem.f_type != 0x9fa0) return fail(owner, MA_PROCFS, 0, proc_fd);
  size_t used = 0;
  int eof = 0;
  for (unsigned int attempts = 0; attempts < MA_IO_LIMIT; ++attempts) {
    unsigned char extra;
    void *destination = used < MA_MAPS_LIMIT ?
      (unsigned char *)owner->maps_storage + used : &extra;
    size_t available = used < MA_MAPS_LIMIT ? MA_MAPS_LIMIT - used : 1;
    result = ma_raw(__NR_read, proc_fd, (long)destination, (long)available, 0, 0, 0);
    if (result == -EINTR) continue;
    if (result < 0) return fail(owner, MA_IO, result, proc_fd);
    if (result == 0) { eof = 1; break; }
    if ((size_t)result > available || used == MA_MAPS_LIMIT)
      return fail(owner, MA_LIMIT, 0, proc_fd);
    used += (size_t)result;
  }
  if (!eof) return fail(owner, MA_LIMIT, 0, proc_fd);
  status = ma_stack_extent(owner->maps_storage, used, saved_rsp, &owner->original_stack);
  if (status != MA_OK) return fail(owner, status, 0, proc_fd);
  result = ma_raw(__NR_close, proc_fd, 0, 0, 0, 0, 0);
  if (result < 0) return fail(owner, MA_CLEANUP, result, -1);
  result = ma_raw(__NR_munmap, (long)owner->maps_storage, MA_MAPS_LIMIT, 0, 0, 0, 0);
  if (result < 0) {
    cleanup_error(owner, result);
    return fail(owner, MA_CLEANUP, result, -1);
  }
  owner->maps_storage = NULL;
  owner->maps_size = 0;
  owner->status = MA_OK;
  return MA_OK;
}
