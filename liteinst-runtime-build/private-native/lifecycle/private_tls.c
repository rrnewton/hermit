#include "private_tls.h"
#include "mapper.h"
#include <asm/prctl.h>
#include <asm/unistd.h>
#include <errno.h>

extern long reverie_preload_trusted_syscall(long, long, long, long, long, long, long)
    __attribute__((visibility("hidden")));

struct pl_tls_control pl_tls;
static const struct pm_crt_context *private_owner;
const struct pl_context_hooks pl_context_hooks = {
  pl_context_enter, pl_context_leave, pl_context_finish_deferred
};

int pl_initial_inputs(const struct pm_crt_context *context, struct pl_initial_inputs *out) {
  if (context == NULL || out == NULL || context->owner == NULL ||
      context->version != PM_CONTEXT_VERSION || context->size != sizeof *context)
    return -EINVAL;
  const struct pm_owner *owner = context->owner;
  if (context != &owner->context || context->stack_handoff != owner->launch.handoff ||
      context->captured != &owner->captured || owner->transferred != 1 ||
      owner->status != PM_OK || owner->original_fd < 3 || owner->runtime_fd < 3 ||
      owner->original_fd == owner->runtime_fd || owner->original_brk == 0 ||
      owner->original_brk >= (UINT64_C(1) << 47)) return -EINVAL;
  const struct ps_handoff *handoff = context->stack_handoff;
  if (handoff == NULL || handoff->version != PS_ABI_VERSION ||
      handoff->size != sizeof *handoff || handoff->original_sp != owner->captured.rsp ||
      handoff->original_auxc > PS_MAX_AUX ||
      handoff->original_stack_end < handoff->original_sp ||
      handoff->original_stack_end - handoff->original_sp != owner->acquired.original_stack.size ||
      handoff->original_auxv < handoff->original_sp ||
      handoff->original_auxv > handoff->original_stack_end) return -EINVAL;
  size_t length = ((size_t)handoff->original_auxc + 1) * 16;
  if (length > handoff->original_stack_end - handoff->original_auxv) return -EINVAL;
  *out = (struct pl_initial_inputs) {
    (const unsigned char *)handoff->original_auxv, length, handoff->original_sp,
    handoff->original_stack_end, owner->original_fd, owner->original_brk
  };
  return 0;
}

int pl_prepare_private_tls(const struct pm_crt_context *context) {
  if (context == NULL || context->version != PM_CONTEXT_VERSION ||
      context->size != sizeof *context || context->owner == NULL) return -EINVAL;
  const struct pm_owner *owner = context->owner;
  if (context != &owner->context || context->captured != &owner->captured ||
      context->stack_handoff != owner->launch.handoff || owner->status != PM_OK ||
      owner->transferred != 1 || owner->image.reservation == NULL || owner->stack == NULL ||
      owner->image.bias != context->runtime_bias || owner->image.span != context->runtime_span ||
      owner->captured.valid != PE_VALID_ALL || owner->captured.failure != 0 ||
      owner->captured.rsp != (uintptr_t)owner->acquired.original_stack.data ||
      context->runtime_span > UINTPTR_MAX - context->runtime_bias ||
      owner->stack_size > UINTPTR_MAX - (uintptr_t)owner->stack) return -EINVAL;
  uintptr_t pc = (uintptr_t)pl_prepare_private_tls;
  uintptr_t sp;
  __asm__ volatile("mov %%rsp, %0" : "=r"(sp));
  if (pc < context->runtime_bias || pc - context->runtime_bias >= context->runtime_span ||
      sp < (uintptr_t)owner->stack || sp - (uintptr_t)owner->stack >= owner->stack_size)
    return -EPERM;
  uint64_t private_fs = 0, current_gs = 0;
  long result = reverie_preload_trusted_syscall(__NR_arch_prctl, ARCH_GET_FS,
      (long)&private_fs, 0, 0, 0, 0);
  if (result != 0) return result < 0 && result >= -4095 ? (int)result : -EIO;
  result = reverie_preload_trusted_syscall(__NR_arch_prctl, ARCH_GET_GS,
      (long)&current_gs, 0, 0, 0, 0);
  if (result != 0) return result < 0 && result >= -4095 ? (int)result : -EIO;
  if (private_fs == 0 || private_fs == owner->captured.fs_base ||
      current_gs != owner->captured.gs_base || *(const uint64_t *)(uintptr_t)private_fs != private_fs)
    return -EPERM;
  long tid = reverie_preload_trusted_syscall(__NR_gettid, 0, 0, 0, 0, 0, 0);
  if (tid <= 0) return tid >= -4095 && tid < 0 ? (int)tid : -EIO;
  uint64_t expected = 0;
  if (!atomic_compare_exchange_strong_explicit(&pl_tls.phase, &expected, 1,
       memory_order_acq_rel, memory_order_acquire)) return -EALREADY;
  atomic_store_explicit(&pl_tls.private_fs, private_fs, memory_order_relaxed);
  atomic_store_explicit(&pl_tls.owner_tid, (uint64_t)tid, memory_order_relaxed);
  atomic_store_explicit(&pl_tls.guest_fs, owner->captured.fs_base, memory_order_relaxed);
  atomic_store_explicit(&pl_tls.private_gs, current_gs, memory_order_relaxed);
  atomic_store_explicit(&pl_tls.guest_gs, owner->captured.gs_base, memory_order_relaxed);
  atomic_store_explicit(&pl_tls.deferred, 0, memory_order_relaxed);
  private_owner = context;
  atomic_store_explicit(&pl_tls.phase, 2, memory_order_release);
  return 0;
}

int pl_is_private_owner(const struct pm_crt_context *context) {
  if (atomic_load_explicit(&pl_tls.phase, memory_order_acquire) != 2 ||
      context == NULL || context != private_owner) return -EPERM;
  long tid = reverie_preload_trusted_syscall(__NR_gettid, 0, 0, 0, 0, 0, 0);
  if (tid <= 0 || (uint64_t)tid != atomic_load_explicit(&pl_tls.owner_tid, memory_order_relaxed))
    return -EPERM;
  uint64_t current_fs = 0;
  long result = reverie_preload_trusted_syscall(__NR_arch_prctl, ARCH_GET_FS,
      (long)&current_fs, 0, 0, 0, 0);
  if (result != 0 || current_fs != atomic_load_explicit(&pl_tls.private_fs, memory_order_relaxed))
    return -EPERM;
  uint64_t current_gs = 0;
  result = reverie_preload_trusted_syscall(__NR_arch_prctl, ARCH_GET_GS,
      (long)&current_gs, 0, 0, 0, 0);
  if (result != 0 || current_gs != atomic_load_explicit(&pl_tls.private_gs, memory_order_relaxed))
    return -EPERM;
  return 0;
}

int pl_file_prefix(const struct pm_plan *plan, size_t span) {
  if (plan == NULL || plan->count > 128 || span == 0 || span > PM_MAX_SPAN ||
      span % PM_PAGE != 0) return -EINVAL;
  size_t covered = 0;
  while (covered < span) {
    size_t next = covered;
    for (size_t index = 0; index < plan->count; ++index) {
      const struct pm_segment *load = &plan->loads[index];
      if (load->filesz == 0) continue;
      if (load->address > UINTPTR_MAX - (PM_PAGE - 1) ||
          load->filesz > UINTPTR_MAX - (PM_PAGE - 1) - load->address) return -EINVAL;
      uintptr_t end = (load->address + load->filesz + PM_PAGE - 1) & ~(uintptr_t)(PM_PAGE - 1);
      if (load->page_begin <= covered && covered < end) {
        next = end < span ? end : span;
        break;
      }
    }
    if (next == covered) return -ENOTSUP;
    covered = next;
  }
  return 0;
}

static int separated(uintptr_t base, size_t span, uintptr_t other, size_t length) {
  if (other == 0 || length == 0 || length > UINTPTR_MAX - other) return 0;
  return base + span <= other || other + length <= base;
}

int pl_destination_disjoint(const struct pm_owner *owner, uintptr_t base, size_t span) {
  if (owner == NULL || base == 0 || base % PM_PAGE != 0 || span == 0 ||
      span > PM_MAX_SPAN || span % PM_PAGE != 0 || span > UINTPTR_MAX - base)
    return -EINVAL;
  if (!separated(base, span, (uintptr_t)owner, sizeof *owner) ||
      !separated(base, span, (uintptr_t)owner->image.reservation, owner->image.reservation_size) ||
      !separated(base, span, (uintptr_t)owner->stack, owner->stack_size) ||
      !separated(base, span, (uintptr_t)owner->acquired.original_stack.data,
                 owner->acquired.original_stack.size) ||
      !separated(base, span, (uintptr_t)owner->acquired.runtime_file.data,
                 owner->acquired.runtime_file.size)) return -EPERM;
  return 0;
}

int pl_validate_original_destination(const struct pm_crt_context *context,
                                    uintptr_t base, size_t span) {
  struct pl_initial_inputs inputs;
  int valid = pl_initial_inputs(context, &inputs);
  if (valid != 0) return valid;
  const struct pm_owner *owner = context->owner;
  if (atomic_load_explicit(&pl_tls.phase, memory_order_acquire) != 2) return -EPERM;
  valid = pl_destination_disjoint(owner, base, span);
  if (valid != 0) return valid;
  struct pm_plan plan;
  if (pm_validate(owner->acquired.runtime_file, owner->acquired.crt_entry_offset, &plan) != PM_OK)
    return -ESTALE;
  valid = pl_file_prefix(&plan, span);
  if (valid != 0) return valid;
  struct ma_owner maps = {0};
  size_t length = 0;
  enum ma_status acquired = pm_read_maps(owner->captured.rsp, &maps, &length);
  if (acquired != MA_OK) {
    long primary = maps.primary_error;
    return primary < 0 && primary >= -4095 ? (int)primary : -EIO;
  }
  enum pm_status binding = pm_check_binding(&owner->captured, owner->runtime_fd,
      owner->acquired.runtime_file, owner->acquired.crt_entry_offset, base,
      (struct ps_view) {maps.maps_storage, length});
  enum ma_status released = ma_release(&maps);
  if (binding != PM_OK) return -ESTALE;
  if (released != MA_OK) {
    long cleanup = maps.cleanup_error;
    return cleanup < 0 && cleanup >= -4095 ? (int)cleanup : -EIO;
  }
  return 0;
}
