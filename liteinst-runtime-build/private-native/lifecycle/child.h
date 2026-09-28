#ifndef PRIVATE_LIFECYCLE_CHILD_H
#define PRIVATE_LIFECYCLE_CHILD_H
#include "entry.h"

#define PL_CHILD_SELF 4416
#define PL_CHILD_PRIVATE_FS 4424
#define PL_CHILD_TERMINAL_TOP 4432
#define PL_CHILD_PRIVATE_TOP 4440
#define PL_CHILD_KERNEL_TID 4448
#define PL_CHILD_PARENT_TID 4452
#define PL_CHILD_EVENT 4456
#define PL_CHILD_STAGE 4460
#define PL_CHILD_INHERITED_FS 4464
#define PL_CHILD_INHERITED_GS 4472
#define PL_CHILD_EXIT_FS 4480
#define PL_CHILD_EXIT_GS 4488
#define PL_CHILD_BOOT_LOW 4496
#define PL_CHILD_BOOT_HIGH 4504
#define PL_CHILD_EXIT_RELEASE 4512
#define PL_CHILD_PIDFD_READY 4516
#define PL_CHILD_FAILED 8
#define PL_CHILD_CLONE_FLAGS 0x1350f00

#ifndef __ASSEMBLER__
#include "gnu_provider.h"
#include <stdatomic.h>
#include <sys/stat.h>

enum pl_child_stage {
  PL_CHILD_EMPTY, PL_CHILD_STARTING, PL_CHILD_ARRIVED, PL_CHILD_ACTIVE,
  PL_CHILD_RETIRING, PL_CHILD_RETIRED, PL_CHILD_EXITING, PL_CHILD_REAPED,
  PL_CHILD_POISONED = PL_CHILD_FAILED
};

struct pl_child;
struct pl_child_borrow {
  struct pl_child *child;
};

struct pl_child {
  struct pe_entry entry;
  struct pl_child *self;
  uintptr_t private_fs, terminal_top, private_top;
  _Atomic uint32_t kernel_tid, parent_tid, event, stage;
  uint64_t inherited_fs, inherited_gs, exit_fs, exit_gs;
  uintptr_t boot_low, boot_high;
  _Atomic uint32_t exit_release, pidfd_ready;
  const struct pm_crt_context *root;
  struct hermit_private_reservation_owner_v1 origin;
  struct hermit_private_reservation_v1 reservation;
  struct hermit_private_native_v1 native;
  struct hermit_private_native_info_v1 information;
  struct hermit_private_stack_v1 bootstrap, terminal;
  struct pl_tls_control tls;
  _Atomic uintptr_t borrow;
  _Atomic int64_t raw_create;
  _Atomic int64_t failure;
  _Atomic uint32_t parent_arrived;
  int pidfd;
  struct stat pidfd_stat;
  uint64_t parent_shstk;
  int64_t parent_shstk_result;
  _Atomic uint32_t entry_release;
  void (*body)(struct pl_child *);
};

struct pl_child_creation {
  int64_t kernel_result;
  int32_t preparation_error, completion_error;
  uint32_t kernel_entered, reserved;
};

#define PL_CHILD_OFFSET(field, offset) \
  _Static_assert(offsetof(struct pl_child, field) == offset, #field " offset")
PL_CHILD_OFFSET(entry, 0);
PL_CHILD_OFFSET(self, PL_CHILD_SELF);
PL_CHILD_OFFSET(private_fs, PL_CHILD_PRIVATE_FS);
PL_CHILD_OFFSET(terminal_top, PL_CHILD_TERMINAL_TOP);
PL_CHILD_OFFSET(private_top, PL_CHILD_PRIVATE_TOP);
PL_CHILD_OFFSET(kernel_tid, PL_CHILD_KERNEL_TID);
PL_CHILD_OFFSET(parent_tid, PL_CHILD_PARENT_TID);
PL_CHILD_OFFSET(event, PL_CHILD_EVENT);
PL_CHILD_OFFSET(stage, PL_CHILD_STAGE);
PL_CHILD_OFFSET(inherited_fs, PL_CHILD_INHERITED_FS);
PL_CHILD_OFFSET(inherited_gs, PL_CHILD_INHERITED_GS);
PL_CHILD_OFFSET(exit_fs, PL_CHILD_EXIT_FS);
PL_CHILD_OFFSET(exit_gs, PL_CHILD_EXIT_GS);
PL_CHILD_OFFSET(boot_low, PL_CHILD_BOOT_LOW);
PL_CHILD_OFFSET(boot_high, PL_CHILD_BOOT_HIGH);
PL_CHILD_OFFSET(exit_release, PL_CHILD_EXIT_RELEASE);
PL_CHILD_OFFSET(pidfd_ready, PL_CHILD_PIDFD_READY);
#undef PL_CHILD_OFFSET
_Static_assert(_Alignof(struct pl_child) == 64, "child capture alignment");
_Static_assert(ATOMIC_INT_LOCK_FREE == 2, "kernel and futex words");
_Static_assert(sizeof(struct hermit_private_native_v1) == 24, "native GNU ticket size");
_Static_assert(sizeof(struct hermit_private_native_info_v1) == 136, "native GNU projection size");
_Static_assert(sizeof(struct pl_child_creation) == 24, "native creation result size");

__attribute__((visibility("hidden")))
struct pl_child_creation pl_child_start(const struct pm_crt_context *root,
    const struct hermit_private_reservation_owner_v1 *origin,
    const struct hermit_private_reservation_v1 *reservation,
    const struct hermit_private_stack_v1 *bootstrap,
    const struct hermit_private_stack_v1 *terminal,
    struct pl_child *child, void (*body)(struct pl_child *));
__attribute__((visibility("hidden")))
int pl_child_wait_arrival(struct pl_child *child);
__attribute__((visibility("hidden")))
int pl_child_release_entry(struct pl_child *child);
__attribute__((visibility("hidden")))
int pl_child_borrow_acquire(struct pl_child *child, struct pl_child_borrow *borrow);
__attribute__((visibility("hidden")))
int pl_child_borrow_release(struct pl_child_borrow *borrow);
__attribute__((visibility("hidden"), noreturn))
void pl_child_retire(struct pl_child *child);
__attribute__((visibility("hidden")))
int pl_child_release_exit(struct pl_child *child);
__attribute__((visibility("hidden")))
int pl_child_reap(struct pl_child *child);
__attribute__((visibility("hidden")))
long pl_child_kernel(struct pl_child *child);
__attribute__((visibility("hidden"), noreturn))
void pl_child_entry(void);
__attribute__((visibility("hidden"), noreturn))
void pl_child_arrived(struct pl_child *child);
__attribute__((visibility("hidden"), noreturn))
void pl_child_terminal(struct pl_child *child);
__attribute__((visibility("hidden"), noreturn))
void pl_child_abandon(struct pl_child *child, long error);
#endif
#endif
