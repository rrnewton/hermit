#define _GNU_SOURCE
#include "mapper.h"
#include <asm/unistd.h>
#include <sys/mman.h>

enum pm_status pm_release(struct pm_owner *owner) {
  if (owner == NULL || owner->transferred) return PM_ARGUMENT;
  int failed = 0;
  if (owner->stack != NULL) {
    long result = ma_raw(__NR_munmap, (long)owner->stack, (long)owner->stack_size, 0, 0, 0, 0);
    if (result == 0) { owner->stack = NULL; owner->stack_size = 0; }
    else {
      failed = 1;
      if (owner->cleanup_error == 0) owner->cleanup_error = result;
    }
  }
  if (pm_unmap(&owner->image) != PM_OK) {
    failed = 1;
    if (owner->cleanup_error == 0) owner->cleanup_error = owner->image.cleanup_error;
  }
  if (ma_release(&owner->acquired) != MA_OK) {
    failed = 1;
    if (owner->cleanup_error == 0) owner->cleanup_error = owner->acquired.cleanup_error;
  }
  if (ma_release(&owner->maps) != MA_OK) {
    failed = 1;
    if (owner->cleanup_error == 0) owner->cleanup_error = owner->maps.cleanup_error;
  }
  return failed ? PM_CLEANUP : PM_OK;
}

enum pm_status pm_prepare(const struct pe_entry *entry, struct ma_owner *acquired,
                         struct pm_owner *owner) {
  if (entry == NULL || acquired == NULL || owner == NULL || owner->transferred ||
      owner->image.reservation != NULL || owner->stack != NULL || owner->acquired.runtime_file.data != NULL ||
      acquired->status != MA_OK || acquired->runtime_file.data == NULL || acquired->maps_storage != NULL ||
      (uintptr_t)acquired->original_stack.data != entry->rsp) return PM_ARGUMENT;
  owner->origin = entry;
  const unsigned char *source = (const unsigned char *)entry;
  unsigned char *destination = (unsigned char *)&owner->captured;
  for (size_t index = 0; index < sizeof *entry; ++index) destination[index] = source[index];
  owner->acquired = *acquired;
  *acquired = (struct ma_owner) {0};
  owner->status = pm_map(owner->acquired.runtime_file, owner->acquired.crt_entry_offset, &owner->image);
  if (owner->status != PM_OK) { owner->primary_error = owner->image.primary_error; goto failure; }
  const unsigned char name[] = "/hermit-private-runtime";
  struct pe_mapped_inputs inputs = {
    owner->acquired.original_stack, owner->acquired.runtime_file,
    owner->image.bias, owner->image.span, owner->acquired.crt_entry_offset,
    {name, sizeof name}, 8u * 1024u * 1024u
  };
  struct ps_request request;
  owner->builder_status = pe_builder_request(entry, &inputs, &request);
  if (owner->builder_status != PS_OK) { owner->status = PM_BUILDER; goto failure; }
  size_t needed;
  owner->builder_status = ps_measure(&request, &needed);
  if (owner->builder_status != PS_OK) { owner->status = PM_BUILDER; goto failure; }
  owner->stack_size = (needed + PM_PAGE - 1) & ~(size_t)(PM_PAGE - 1);
  long result = ma_raw(__NR_mmap, 0, (long)owner->stack_size, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if ((unsigned long)result >= (unsigned long)-4095) {
    owner->stack_size = 0; owner->primary_error = result; owner->status = PM_IO; goto failure;
  }
  owner->stack = (void *)result;
  owner->builder_status = ps_build(&request, owner->stack, owner->stack_size, &owner->launch);
  if (owner->builder_status != PS_OK) { owner->status = PM_BUILDER; goto failure; }
  owner->context = (struct pm_crt_context) {
    PM_VERSION, sizeof owner->context, &owner->captured, owner->launch.handoff,
    owner->image.bias, owner->image.span, owner
  };
  owner->status = PM_OK;
  return PM_OK;
failure:
  (void)pm_release(owner);
  return owner->status;
}
