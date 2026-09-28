#include "initial_transfer.h"
#include "mapper.h"
#include <asm/unistd.h>
#include <errno.h>

extern long reverie_preload_trusted_syscall(long, long, long, long, long, long, long)
    __attribute__((visibility("hidden")));
extern const unsigned char pl_initial_fault[] __attribute__((visibility("hidden")));
struct pe_entry pl_initial_capture __attribute__((visibility("hidden")));
static struct pl_initial_record initial_record;
static _Atomic unsigned int initial_phase;

int pl_arm_initial(const struct pm_crt_context *context, uintptr_t loader_entry) {
  int result = pl_is_private_owner(context);
  if (result != 0) return result;
  struct pl_initial_inputs inputs;
  result = pl_initial_inputs(context, &inputs);
  if (result != 0) return result;
  const struct pe_entry *capture = context->captured;
  if (capture->version != PE_VERSION || capture->size != sizeof *capture ||
      capture->valid != PE_VALID_ALL || capture->failure != 0 ||
      capture->xcr0 != PE_XSTATE_MASK || capture->xstate_size != 2440 ||
      capture->rflags_image & (0x100 | 0x10000 | 0x20000) ||
      (capture->rflags_image & 0x202u) != 0x202u ||
      capture->fs != 0 || capture->gs != 0 || capture->ds != 0 || capture->es != 0 ||
      capture->cs != 0x33 || capture->ss != 0x2b ||
      loader_entry == 0 || loader_entry >= (UINT64_C(1) << 47)) return -ENOTSUP;
  unsigned int expected = 0;
  if (!atomic_compare_exchange_strong_explicit(&initial_phase, &expected, 1,
      memory_order_acq_rel, memory_order_acquire)) return -EALREADY;
  for (size_t index = 0; index < sizeof *capture; ++index)
    ((unsigned char *)&pl_initial_capture)[index] = ((const unsigned char *)capture)[index];
  initial_record = (struct pl_initial_record) {
    &pl_initial_capture, loader_entry, (uintptr_t)pl_initial_fault
  };
  const struct pm_owner *owner = context->owner;
  long closed = reverie_preload_trusted_syscall(__NR_close, inputs.original_fd, 0, 0, 0, 0, 0);
  if (closed != 0) return closed < 0 && closed >= -4095 ? (int)closed : -EIO;
  closed = reverie_preload_trusted_syscall(__NR_close, owner->runtime_fd, 0, 0, 0, 0, 0);
  if (closed != 0) return closed < 0 && closed >= -4095 ? (int)closed : -EIO;
  atomic_store_explicit(&initial_phase, 2, memory_order_release);
  return 0;
}

const struct pl_initial_record *pl_take_initial(void) {
  unsigned int expected = 2;
  if (!atomic_compare_exchange_strong_explicit(&initial_phase, &expected, 3,
      memory_order_acq_rel, memory_order_acquire)) return NULL;
  return &initial_record;
}
