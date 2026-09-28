#define _GNU_SOURCE
#include "mapper.h"
#include <asm/unistd.h>
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/stat.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/vfs.h>

static unsigned char golden[PM_BINDING_LIMIT], record[PM_BINDING_LIMIT];
static _Alignas(4096) unsigned char stack[4096];
static size_t record_size;
static unsigned int calls, proofs, unmounts, closes, fail_at, controls;
static int detached, wrong_namespace, wrong_top, wrong_restored_mount, unsealed, read_interrupt;
static int source_removed, restored_checked, wrong_runtime_mount;
static enum pm_status proof_result;

static uint64_t get64(const unsigned char *bytes) {
  uint64_t value = 0;
  for (size_t index = 0; index < 8; ++index) value |= (uint64_t)bytes[index] << (index * 8);
  return value;
}

static void put64(unsigned char *bytes, uint64_t value) {
  for (size_t index = 0; index < 8; ++index) bytes[index] = (unsigned char)(value >> (index * 8));
}

enum pm_status pm_check_binding(const struct pe_entry *entry, int fd, struct ps_view file,
                               uintptr_t crt, uintptr_t base, struct ps_view maps) {
  assert(entry != NULL && fd == 10 && base == 0x4000);
  (void)file; (void)crt; (void)maps;
  ++proofs;
  return proof_result;
}

enum ma_status ma_stack_extent(const unsigned char *maps, size_t size, uintptr_t rsp, struct ps_view *view) {
  (void)maps; (void)size; (void)rsp; (void)view; abort();
}
enum ma_status ma_release(struct ma_owner *owner) { (void)owner; abort(); }

static struct statx metadata(uint64_t inode, uint64_t mount) {
  struct statx stat = {0};
  stat.stx_mask = STATX_BASIC_STATS | STATX_MNT_ID;
  stat.stx_mode = S_IFREG | 0755;
  stat.stx_dev_major = 8; stat.stx_dev_minor = 1;
  stat.stx_ino = inode; stat.stx_mnt_id = mount; stat.stx_size = 4096;
  stat.stx_mtime.tv_sec = 19; stat.stx_mtime.tv_nsec = 23;
  stat.stx_ctime.tv_sec = 29; stat.stx_ctime.tv_nsec = 31;
  return stat;
}

long ma_raw(long number, long first, long second, long third, long fourth, long fifth, long sixth) {
  (void)sixth;
  ++calls;
  assert(proofs == 1 && proof_result == PM_OK);
  if (calls == fail_at) return -EIO;
  if (number == __NR_openat) { assert(strcmp((const char *)second, "/proc/thread-self/ns/mnt") == 0); return 42; }
  if (number == __NR_fstatfs) { assert(first == 42 || first == 14); ((struct statfs *)second)->f_type = first == 42 ? 0x6e736673 : 0x01021994; return 0; }
  if (number == __NR_ioctl) { assert(first == 42); return CLONE_NEWNS; }
  if (number == __NR_fcntl) {
    if (second == F_GET_SEALS) return unsealed ? 0 : F_SEAL_SEAL | F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_WRITE;
    assert(second == F_GETFL); return first == 12 || first == 14 ? O_PATH : -EBADF;
  }
  if (number == __NR_statx) {
    assert(fourth == (STATX_BASIC_STATS | STATX_MNT_ID));
    const char *path = (const char *)second;
    struct statx stat;
    if (first == 13) { stat = metadata(505, 7); stat.stx_size = record_size; }
    else if (first == 10) stat = metadata(101, 0);
    else if (first == 14) {
      if (strcmp(path, "runtime") == 0) {
        if (source_removed) return -ENOENT;
        stat = metadata(707, 17); stat.stx_mode = S_IFLNK | 0777;
      } else { assert(path[0] == 0); stat = metadata(606, 17); stat.stx_mode = S_IFDIR | 0700; }
    }
    else if (first == 12) stat = metadata(202, 81);
    else if (first == 42) stat = metadata(wrong_namespace ? 303 : 404, 7);
    else {
      if (strcmp(path, "/lib/loader") != 0) return -ENOENT;
      if (detached) { assert(third == AT_SYMLINK_NOFOLLOW); stat = metadata(202, wrong_restored_mount ? 51 : 81); }
      else if (third == 0) stat = metadata(101, wrong_runtime_mount ? 91 : 0);
      else { assert(third == AT_SYMLINK_NOFOLLOW); stat = metadata(707, wrong_top ? 92 : 91); stat.stx_mode = S_IFLNK | 0777; }
    }
    *(struct statx *)fifth = stat;
    return 0;
  }
  if (number == __NR_pread64) {
    assert(first == 13 && fourth >= 0 && (size_t)fourth <= record_size);
    if (read_interrupt) { --read_interrupt; return -EINTR; }
    size_t count = (size_t)third;
    if (count > 37) count = 37;
    if (count > record_size - (size_t)fourth) count = record_size - (size_t)fourth;
    memcpy((void *)second, record + (size_t)fourth, count);
    return (long)count;
  }
  if (number == __NR_umount2) {
    assert(strcmp((const char *)first, "/lib/loader") == 0 && second == (MNT_DETACH | UMOUNT_NOFOLLOW) && !detached);
    ++unmounts; detached = 1; return 0;
  }
  if (number == __NR_faccessat2) { assert(detached && first == 12); restored_checked = 1; return 0; }
  if (number == __NR_readlinkat) {
    const char text[] = "/proc/self/fd/10";
    assert(first == 14 && strcmp((const char *)second, "runtime") == 0 && fourth >= (long)sizeof text);
    memcpy((void *)third, text, sizeof text - 1); return sizeof text - 1;
  }
  if (number == __NR_unlinkat) {
    assert(detached && restored_checked && first == 14 && strcmp((const char *)second, "runtime") == 0 && third == 0);
    source_removed = 1; return 0;
  }
  if (number == __NR_close) { if (first == 42) return 0; assert(detached && restored_checked && source_removed && (first == 12 || first == 13 || first == 14)); ++closes; return 0; }
  fprintf(stderr, "unmodeled syscall %ld\n", number);
  abort();
}

static void reset(void) {
  memcpy(record, golden, sizeof record);
  memset(stack, 0, sizeof stack);
  strcpy((char *)stack + 256, PM_BINDING_ENV "=13");
  put64(stack + 16, (uintptr_t)stack + 256);
  put64(stack + 32, 7); put64(stack + 40, 0x4000);
  calls = proofs = unmounts = closes = fail_at = 0;
  detached = wrong_namespace = wrong_top = wrong_restored_mount = unsealed = read_interrupt = 0;
  source_removed = restored_checked = wrong_runtime_mount = 0;
  proof_result = PM_OK;
}

static enum pm_status invoke(struct pm_owner *owner) {
  struct pe_entry entry = {0}; entry.rsp = (uintptr_t)stack;
  struct ma_owner input = {0};
  owner->runtime_fd = 10; owner->original_fd = 11;
  owner->maps.original_stack = (struct ps_view){stack, sizeof stack};
  return pm_restore_interpreter_binding(&entry, owner, &input, 0x4000, 0);
}

static void control(const char *name) { ++controls; printf("ok: %s\n", name); }

int main(int argc, char **argv) {
  assert(argc == 2);
  FILE *file = fopen(argv[1], "rb"); assert(file);
  record_size = fread(golden, 1, sizeof golden, file);
  assert(!ferror(file) && fclose(file) == 0 && record_size == PM_BINDING_HEADER + 12);
  assert(get64(golden + 88) == 81 && get64(golden + 96) == 91);
  struct pm_owner owner = {0}; reset(); read_interrupt = 2;
  assert(invoke(&owner) == PM_OK && owner.restoration_complete && unmounts == 1 && closes == 3 && source_removed);
  control("Rust golden record consumed; short/interrupted reads; cloned original mount restored");
  assert(invoke(&owner) == PM_ARGUMENT);
  control("restoration cannot be reused");
  reset(); owner = (struct pm_owner){0}; proof_result = PM_BINDING;
  assert(invoke(&owner) == PM_BINDING && calls == 0 && !owner.restoration_complete);
  control("genuine mapping proof precedes every record syscall");
  for (size_t index = 0; index < record_size; ++index) {
    if (index >= 136 && index < 152) continue;
    reset(); owner = (struct pm_owner){0}; record[index] ^= 0x80;
    enum pm_status status = invoke(&owner);
    assert(status != PM_OK && !owner.restoration_complete);
  }
  control("every identity-checked golden byte mutation rejected");
  for (int which = 0; which < 4; ++which) {
    reset(); owner = (struct pm_owner){0};
    if (which == 0) wrong_namespace = 1;
    if (which == 1) wrong_top = 1;
    if (which == 2) wrong_restored_mount = 1;
    if (which == 3) unsealed = 1;
    assert(invoke(&owner) != PM_OK && !owner.restoration_complete);
    assert(unmounts == (which == 2 ? 1u : 0u));
  }
  control("wrong namespace, stacked mount, pre-clone restored mount and unsealed record rejected");
  reset(); owner = (struct pm_owner){0}; wrong_runtime_mount = 1;
  assert(invoke(&owner) == PM_BINDING && !unmounts && !source_removed);
  control("runtime mount zero is exact, never a wildcard or alias attachment identity");
  reset(); owner = (struct pm_owner){0}; assert(invoke(&owner) == PM_OK);
  unsigned int total = calls;
  for (unsigned int index = 1; index <= total; ++index) {
    reset(); owner = (struct pm_owner){0}; fail_at = index;
    assert(invoke(&owner) != PM_OK && !owner.restoration_complete);
    if (!restored_checked) assert(!source_removed);
  }
  control("all raw operation failure points refuse completion, including unmount and close");
  for (size_t index = 0; index < 4; ++index) {
    const char *bad[] = {PM_BINDING_ENV "=013", PM_BINDING_ENV "=2", PM_BINDING_ENV "=2147483648", PM_BINDING_ENV "="};
    reset(); owner = (struct pm_owner){0}; strcpy((char *)stack + 256, bad[index]);
    assert(invoke(&owner) == PM_DISCOVERY && calls == 0);
  }
  reset(); owner = (struct pm_owner){0}; put64(stack + 24, (uintptr_t)stack + 256);
  assert(invoke(&owner) == PM_DISCOVERY && calls == 0);
  control("noncanonical, overflowing, reserved and duplicate discovery descriptors rejected");
  printf("%u modeled restoration controls; no real raw syscalls\n", controls);
}
