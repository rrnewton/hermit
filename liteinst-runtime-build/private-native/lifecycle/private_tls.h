#ifndef PRIVATE_LIFECYCLE_TLS_H
#define PRIVATE_LIFECYCLE_TLS_H
#include <stdint.h>
#include <stdatomic.h>
#include "private_crt_context.h"

struct pl_tls_control {
  _Atomic uint64_t private_fs;
  _Atomic uint64_t owner_tid;
  _Atomic uint64_t guest_fs;
  _Atomic uint64_t deferred;
  _Atomic uint64_t phase;
  _Atomic uint64_t private_gs;
  _Atomic uint64_t guest_gs;
};

struct pl_context_hooks {
  uint64_t (*enter)(void);
  void (*leave)(uint64_t, uint64_t);
  void (*finish_deferred)(void);
};

_Static_assert(ATOMIC_LONG_LOCK_FREE == 2, "lock-free native control words");
_Static_assert(sizeof(struct pl_tls_control) == 56, "TLS control size");
_Static_assert(offsetof(struct pl_tls_control, private_gs) == 40, "private GS offset");
_Static_assert(offsetof(struct pl_tls_control, guest_gs) == 48, "guest GS offset");
_Static_assert(offsetof(struct pl_tls_control, private_fs) == 0, "private FS offset");
_Static_assert(offsetof(struct pl_tls_control, owner_tid) == 8, "TID offset");
_Static_assert(offsetof(struct pl_tls_control, guest_fs) == 16, "guest FS offset");
_Static_assert(offsetof(struct pl_tls_control, deferred) == 24, "continuation offset");
_Static_assert(offsetof(struct pl_tls_control, phase) == 32, "phase offset");
_Static_assert(sizeof(struct pl_context_hooks) == 24, "context hooks size");

extern struct pl_tls_control pl_tls __attribute__((visibility("hidden")));
struct pl_tls_observation {
  int64_t result;
  uint64_t guest_fs, guest_gs;
};
_Static_assert(sizeof(struct pl_tls_observation) == 24, "TLS observation size");
_Static_assert(offsetof(struct pl_tls_observation, guest_fs) == 8, "observed FS offset");
_Static_assert(offsetof(struct pl_tls_observation, guest_gs) == 16, "observed GS offset");
void pl_guest_arch_prctl(uint64_t operation, uint64_t argument,
    struct pl_tls_observation *out) __attribute__((visibility("hidden")));
extern const struct pl_context_hooks pl_context_hooks __attribute__((visibility("hidden")));

struct pl_initial_inputs {
  const unsigned char *auxv;
  size_t auxv_bytes;
  uintptr_t stack_begin, stack_end;
  int original_fd;
  uintptr_t original_brk;
};
_Static_assert(sizeof(struct pl_initial_inputs) == 48, "initial inputs size");
_Static_assert(offsetof(struct pl_initial_inputs, original_brk) == 40, "initial brk offset");

__attribute__((visibility("hidden")))
int pl_prepare_private_tls(const struct pm_crt_context *context);
__attribute__((visibility("hidden")))
int pl_is_private_owner(const struct pm_crt_context *context);
__attribute__((visibility("hidden")))
int pl_initial_inputs(const struct pm_crt_context *context, struct pl_initial_inputs *out);
__attribute__((visibility("hidden")))
int pl_validate_original_destination(const struct pm_crt_context *context,
                                    uintptr_t base, size_t span);
struct pm_plan;
struct pm_owner;
__attribute__((visibility("hidden")))
uint64_t pl_fs_role(uint64_t private_fs, uint64_t guest_fs, uint64_t observed_fs);
__attribute__((visibility("hidden")))
int pl_file_prefix(const struct pm_plan *plan, size_t span);
__attribute__((visibility("hidden")))
int pl_destination_disjoint(const struct pm_owner *owner, uintptr_t base, size_t span);
__attribute__((visibility("hidden")))
uint64_t pl_context_enter(void);
__attribute__((visibility("hidden")))
void pl_context_leave(uint64_t token, uint64_t kind);
__attribute__((visibility("hidden")))
void pl_context_finish_deferred(void);
__attribute__((visibility("hidden")))
uint64_t pl_context_enter_control(struct pl_tls_control *control);
__attribute__((visibility("hidden")))
void pl_context_leave_control(struct pl_tls_control *control, uint64_t token,
                              uint64_t kind);
__attribute__((visibility("hidden")))
void pl_context_finish_deferred_control(struct pl_tls_control *control);
#endif
