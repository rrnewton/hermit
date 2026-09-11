#define _GNU_SOURCE
#include "mapper.h"
#include <asm/unistd.h>
#include <sys/mman.h>

static __attribute__((noreturn)) void terminal(void) {
  (void)ma_raw(__NR_exit_group, 127, 0, 0, 0, 0, 0);
  __builtin_trap();
}

void pe_continue(const struct pe_entry *entry) {
  uintptr_t original_brk;
  if (pm_read_initial_brk(&original_brk) != 0) terminal();
  size_t owner_size = (sizeof(struct pm_owner) + PM_PAGE - 1) & ~(size_t)(PM_PAGE - 1);
  long allocated = ma_raw(__NR_mmap, 0, (long)owner_size, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if ((unsigned long)allocated >= (unsigned long)-4095) terminal();
  struct pm_owner *owner = (void *)allocated;
  owner->original_brk = original_brk;
  owner->origin = entry;
  for (size_t index = 0; index < sizeof *entry; ++index)
    ((unsigned char *)&owner->captured)[index] = ((const unsigned char *)entry)[index];
  size_t maps_size;
  owner->acquisition_status = pm_read_maps(entry->rsp, &owner->maps, &maps_size);
  if (owner->acquisition_status != MA_OK) { owner->status = PM_ACQUIRE; goto failure; }
  uintptr_t at_base;
  owner->status = pm_discover(owner->maps.original_stack, &owner->runtime_fd, &at_base);
  if (owner->status != PM_OK) goto failure;
  uintptr_t original_base;
  owner->status = pm_discover_original(owner->maps.original_stack, &owner->original_fd, &original_base);
  if (owner->status != PM_OK) goto failure;
  if (original_base != at_base || owner->original_fd == owner->runtime_fd) {
    owner->status = PM_BINDING; goto failure;
  }
  struct ma_owner input = {0};
  owner->acquisition_status = ma_acquire(entry->rsp, owner->runtime_fd, &input);
  if (owner->acquisition_status != MA_OK) { owner->acquired = input; owner->status = PM_ACQUIRE; goto failure; }
  if (input.original_stack.size != owner->maps.original_stack.size) {
    owner->acquired = input; owner->status = PM_BINDING; goto failure;
  }
  owner->status = pm_restore_interpreter_binding(entry, owner, &input, at_base, maps_size);
  if (owner->status != PM_OK || !owner->restoration_complete) {
    owner->acquired = input; goto failure;
  }
  if (ma_release(&owner->maps) != MA_OK) {
    owner->acquired = input; owner->status = PM_CLEANUP; goto failure;
  }
  owner->status = pm_prepare(entry, &input, owner);
  if (owner->status != PM_OK) {
    if (input.runtime_file.data != NULL) owner->acquired = input;
    goto failure;
  }
  owner->transferred = 1;
  pm_enter_private_crt(&owner->context, owner->launch.crt_entry, owner->launch.crt_sp);
failure:
  (void)pm_release(owner);
  terminal();
}
