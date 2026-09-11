#define _GNU_SOURCE
#include "mapper.h"
#include <asm/unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/stat.h>
#include <linux/nsfs.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <unistd.h>

#define REQUIRED_SEALS (F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE)
#define REQUIRED_STAT (STATX_BASIC_STATS | STATX_MNT_ID)

static uint32_t get32(const unsigned char *bytes) {
  uint32_t result = 0;
  for (unsigned int index = 0; index < 4; ++index) result |= (uint32_t)bytes[index] << (index * 8);
  return result;
}

static uint64_t get64(const unsigned char *bytes) {
  uint64_t result = 0;
  for (unsigned int index = 0; index < 8; ++index) result |= (uint64_t)bytes[index] << (index * 8);
  return result;
}

static uint64_t device(const struct statx *stat) {
  return ((uint64_t)stat->stx_dev_major << 32) | stat->stx_dev_minor;
}

static long stat_at(int fd, const char *path, int flags, struct statx *stat) {
  long result = ma_raw(__NR_statx, fd, (long)path, flags, REQUIRED_STAT, (long)stat, 0);
  if (result == 0 && (stat->stx_mask & REQUIRED_STAT) != REQUIRED_STAT) return -ENOTSUP;
  return result;
}

static long namespace_stat(struct pm_owner *owner, struct statx *stat) {
  long opened = ma_raw(__NR_openat, AT_FDCWD, (long)"/proc/thread-self/ns/mnt", O_RDONLY | O_CLOEXEC, 0, 0, 0);
  if (opened < 0) return opened;
  int descriptor = (int)opened;
  struct statfs filesystem;
  long result = ma_raw(__NR_fstatfs, descriptor, (long)&filesystem, 0, 0, 0, 0);
  if (result == 0 && filesystem.f_type != 0x6e736673) result = -EXDEV;
  if (result == 0) {
    result = ma_raw(__NR_ioctl, descriptor, NS_GET_NSTYPE, 0, 0, 0, 0);
    if (result >= 0) result = result == CLONE_NEWNS ? 0 : -EXDEV;
  }
  if (result == 0) result = stat_at(descriptor, "", AT_EMPTY_PATH, stat);
  long closed = ma_raw(__NR_close, descriptor, 0, 0, 0, 0, 0);
  if (closed < 0) { owner->cleanup_error = closed; if (result == 0) result = closed; }
  return result;
}

static int same_runtime(const unsigned char *bytes, const struct statx *stat) {
  return stat->stx_dev_major == get32(bytes + 48) && stat->stx_dev_minor == get32(bytes + 52) &&
    stat->stx_ino == get64(bytes + 56) && (stat->stx_mode & S_IFMT) == S_IFREG &&
    stat->stx_mnt_id == get64(bytes + 152);
}

static int same_alias(const unsigned char *bytes, const struct statx *stat) {
  return device(stat) == get64(bytes + 160) && stat->stx_ino == get64(bytes + 168) &&
    (stat->stx_mode & S_IFMT) == S_IFLNK;
}

static size_t runtime_path(int fd, char output[64]) {
  const char prefix[] = "/proc/self/fd/";
  size_t used = 0;
  for (; used < sizeof(prefix) - 1; ++used) output[used] = prefix[used];
  char digits[10];
  size_t count = 0;
  unsigned int value = (unsigned int)fd;
  do { digits[count++] = (char)('0' + value % 10); value /= 10; } while (value != 0);
  while (count != 0) output[used++] = digits[--count];
  output[used] = 0;
  return used;
}

static int same_original(const unsigned char *bytes, const struct statx *stat) {
  return stat->stx_dev_major == get32(bytes + 64) && stat->stx_dev_minor == get32(bytes + 68) &&
    stat->stx_ino == get64(bytes + 72) && stat->stx_mode == get32(bytes + 80) &&
    stat->stx_mnt_id == get64(bytes + 88) && stat->stx_size == get64(bytes + 104) &&
    (uint64_t)stat->stx_mtime.tv_sec == get64(bytes + 112) && stat->stx_mtime.tv_nsec == get32(bytes + 120) &&
    stat->stx_ctime.tv_nsec == get32(bytes + 124) && (uint64_t)stat->stx_ctime.tv_sec == get64(bytes + 128);
}

static enum pm_status restore(struct pm_owner *owner, int descriptor) {
  unsigned char bytes[PM_BINDING_LIMIT];
  long result = ma_raw(__NR_fcntl, descriptor, F_GET_SEALS, 0, 0, 0, 0);
  if (result < 0) goto io_failure;
  if ((result & REQUIRED_SEALS) != REQUIRED_SEALS) return PM_BINDING;
  struct statx stat;
  result = stat_at(descriptor, "", AT_EMPTY_PATH, &stat);
  if (result < 0) goto io_failure;
  if ((stat.stx_mode & S_IFMT) != S_IFREG || stat.stx_size < PM_BINDING_HEADER + 2 ||
      stat.stx_size > PM_BINDING_LIMIT) return PM_BINDING;
  size_t length = (size_t)stat.stx_size, used = 0;
  for (unsigned int attempt = 0; attempt < 32 && used < length; ++attempt) {
    result = ma_raw(__NR_pread64, descriptor, (long)(bytes + used), (long)(length - used), (long)used, 0, 0);
    if (result == -EINTR) continue;
    if (result < 0) goto io_failure;
    if (result == 0 || (size_t)result > length - used) return PM_BINDING;
    used += (size_t)result;
  }
  if (used != length) return PM_BINDING;
  const unsigned char magic[] = "HLBIND02";
  for (size_t index = 0; index < 8; ++index) if (bytes[index] != magic[index]) return PM_BINDING;
  if (get32(bytes + 8) != 2 || get32(bytes + 12) != length || get32(bytes + 180) != 0) return PM_BINDING;
  uint32_t fds[5];
  for (size_t index = 0; index < 5; ++index) {
    fds[index] = get32(bytes + (index == 4 ? 176 : 16 + index * 4));
    if (fds[index] < 3 || fds[index] > INT_MAX) return PM_BINDING;
    for (size_t previous = 0; previous < index; ++previous) if (fds[index] == fds[previous]) return PM_BINDING;
  }
  if (fds[0] != (uint32_t)owner->runtime_fd || fds[1] != (uint32_t)owner->original_fd ||
      fds[3] != (uint32_t)descriptor) return PM_BINDING;
  uint32_t path_size = get32(bytes + 84);
  if (path_size == 0 || path_size > 4095 || length != PM_BINDING_HEADER + path_size + 1 ||
      bytes[PM_BINDING_HEADER] != '/' || bytes[length - 1] != 0 ||
      (get32(bytes + 80) & (S_IFMT | S_ISUID | S_ISGID)) != S_IFREG ||
      get32(bytes + 120) >= 1000000000 || get32(bytes + 124) >= 1000000000 ||
      get64(bytes + 88) == 0 || get64(bytes + 96) == 0 || get64(bytes + 88) == get64(bytes + 96)) return PM_BINDING;
  for (size_t index = PM_BINDING_HEADER; index < length - 1; ++index) if (bytes[index] == 0) return PM_BINDING;
  if (get64(bytes + 32) == get64(bytes + 136) && get64(bytes + 40) == get64(bytes + 144)) return PM_BINDING;
  result = namespace_stat(owner, &stat);
  if (result < 0) goto io_failure;
  if (device(&stat) != get64(bytes + 32) || stat.stx_ino != get64(bytes + 40)) return PM_BINDING;
  result = stat_at(owner->runtime_fd, "", AT_EMPTY_PATH, &stat);
  if (result < 0) goto io_failure;
  if (!same_runtime(bytes, &stat)) return PM_BINDING;
  result = ma_raw(__NR_fcntl, owner->runtime_fd, F_GET_SEALS, 0, 0, 0, 0);
  if (result < 0) goto io_failure;
  if ((result & REQUIRED_SEALS) != REQUIRED_SEALS) return PM_BINDING;
  result = ma_raw(__NR_fcntl, owner->original_fd, F_GET_SEALS, 0, 0, 0, 0);
  if (result < 0) goto io_failure;
  if ((result & REQUIRED_SEALS) != REQUIRED_SEALS) return PM_BINDING;
  int original = (int)fds[2];
  result = ma_raw(__NR_fcntl, original, F_GETFL, 0, 0, 0, 0);
  if (result < 0) goto io_failure;
  if ((result & O_PATH) == 0) return PM_BINDING;
  result = stat_at(original, "", AT_EMPTY_PATH, &stat);
  if (result < 0) goto io_failure;
  if (!same_original(bytes, &stat)) return PM_BINDING;
  int source = (int)fds[4];
  result = ma_raw(__NR_fcntl, source, F_GETFL, 0, 0, 0, 0);
  if (result < 0) goto io_failure;
  if ((result & O_PATH) == 0) return PM_BINDING;
  struct statfs filesystem;
  result = ma_raw(__NR_fstatfs, source, (long)&filesystem, 0, 0, 0, 0);
  if (result < 0) goto io_failure;
  if (filesystem.f_type != 0x01021994) return PM_BINDING;
  result = stat_at(source, "", AT_EMPTY_PATH, &stat);
  if (result < 0) goto io_failure;
  if (device(&stat) != get64(bytes + 184) || stat.stx_ino != get64(bytes + 192) ||
      stat.stx_mnt_id != get64(bytes + 200) || (stat.stx_mode & S_IFMT) != S_IFDIR) return PM_BINDING;
  result = stat_at(source, "runtime", AT_SYMLINK_NOFOLLOW, &stat);
  if (result < 0) goto io_failure;
  if (!same_alias(bytes, &stat)) return PM_BINDING;
  char expected[64], link[64];
  size_t expected_size = runtime_path(owner->runtime_fd, expected);
  result = ma_raw(__NR_readlinkat, source, (long)"runtime", (long)link, sizeof link, 0, 0);
  if (result < 0) goto io_failure;
  if ((size_t)result != expected_size) return PM_BINDING;
  for (size_t index = 0; index < expected_size; ++index) if (link[index] != expected[index]) return PM_BINDING;
  const char *target = (const char *)bytes + PM_BINDING_HEADER;
  result = stat_at(AT_FDCWD, target, AT_SYMLINK_NOFOLLOW, &stat);
  if (result < 0) goto io_failure;
  if (!same_alias(bytes, &stat) || stat.stx_mnt_id != get64(bytes + 96)) return PM_BINDING;
  result = stat_at(AT_FDCWD, target, 0, &stat);
  if (result < 0) goto io_failure;
  if (!same_runtime(bytes, &stat)) return PM_BINDING;
  result = ma_raw(__NR_umount2, (long)target, MNT_DETACH | UMOUNT_NOFOLLOW, 0, 0, 0, 0);
  if (result < 0) goto io_failure;
  result = stat_at(AT_FDCWD, target, AT_SYMLINK_NOFOLLOW, &stat);
  if (result < 0) goto io_failure;
  if (!same_original(bytes, &stat)) return PM_BINDING;
  result = ma_raw(__NR_faccessat2, original, (long)"", X_OK, AT_EMPTY_PATH | AT_EACCESS, 0, 0);
  if (result < 0) goto io_failure;
  result = ma_raw(__NR_unlinkat, source, (long)"runtime", 0, 0, 0, 0);
  if (result < 0) { owner->cleanup_error = result; return PM_CLEANUP; }
  result = stat_at(source, "runtime", AT_SYMLINK_NOFOLLOW, &stat);
  if (result != -ENOENT) { owner->cleanup_error = result < 0 ? result : -ESTALE; return PM_CLEANUP; }
  const int temporary[] = {source, original, descriptor};
  for (size_t index = 0; index < sizeof temporary / sizeof temporary[0]; ++index) {
    result = ma_raw(__NR_close, temporary[index], 0, 0, 0, 0, 0);
    if (result < 0 && owner->cleanup_error == 0) owner->cleanup_error = result;
  }
  if (owner->cleanup_error != 0) return PM_CLEANUP;
  owner->restoration_complete = 1;
  return PM_OK;
io_failure:
  owner->primary_error = result;
  return PM_IO;
}

enum pm_status pm_restore_interpreter_binding(const struct pe_entry *entry, struct pm_owner *owner,
                                            const struct ma_owner *input, uintptr_t at_base, size_t maps_size) {
  if (entry == NULL || owner == NULL || input == NULL || owner->restoration_complete || owner->transferred ||
      owner->runtime_fd < 3 || owner->original_fd < 3 || owner->runtime_fd == owner->original_fd ||
      maps_size > owner->maps.maps_size) return PM_ARGUMENT;
  enum pm_status proof = pm_check_binding(entry, owner->runtime_fd, input->runtime_file,
    input->crt_entry_offset, at_base, (struct ps_view) {owner->maps.maps_storage, maps_size});
  if (proof != PM_OK) return proof;
  int descriptor = -1;
  uintptr_t discovered_base = 0;
  enum pm_status discovery = pm_discover_restoration(owner->maps.original_stack, &descriptor, &discovered_base);
  if (discovery != PM_OK) return discovery;
  if (discovered_base != at_base || descriptor == owner->runtime_fd || descriptor == owner->original_fd) return PM_BINDING;
  return restore(owner, descriptor);
}
