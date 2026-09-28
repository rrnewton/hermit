#ifndef PRIVATE_LIFECYCLE_GNU_PROVIDER_H
#define PRIVATE_LIFECYCLE_GNU_PROVIDER_H
#include "private_tls.h"
#include <hermit-private-context.h>

enum pl_gnu_stage {
  PL_GNU_ROOT = 1, PL_GNU_DOMAIN, PL_GNU_ADOPT, PL_GNU_ROOT_QUERY,
  PL_GNU_PREPARE, PL_GNU_QUERY, PL_GNU_ROLLBACK, PL_GNU_CLOSE, PL_GNU_CHECK,
  PL_GNU_RESERVE, PL_GNU_RESERVATION_QUERY, PL_GNU_CANCEL_RESERVATION,
  PL_GNU_CHILD_AUTHORIZE, PL_GNU_CHILD_POISON
};
enum pl_gnu_error {
  PL_GNU_OK, PL_GNU_PHASE, PL_GNU_OWNER, PL_GNU_INVALID,
  PL_GNU_EXHAUSTED, PL_GNU_MISMATCH
};
struct pl_gnu_result {
  uint32_t stage, domain, code, reserved;
};

__attribute__((visibility("hidden")))
struct pl_gnu_result pl_gnu_child_authorize(const struct pm_crt_context *context,
    const struct hermit_private_reservation_owner_v1 *origin);
__attribute__((visibility("hidden")))
struct pl_gnu_result pl_gnu_child_poison(const struct pm_crt_context *context,
    const struct hermit_private_reservation_owner_v1 *origin);

#define PL_GNU_OFFSET(type, field, value) \
  _Static_assert(offsetof(struct type, field) == value, #type "." #field)
_Static_assert(sizeof(struct hermit_private_stack_v1) == 80, "stack ABI size");
PL_GNU_OFFSET(hermit_private_stack_v1, version, 0);
PL_GNU_OFFSET(hermit_private_stack_v1, size, 4);
PL_GNU_OFFSET(hermit_private_stack_v1, allocation_id, 8);
PL_GNU_OFFSET(hermit_private_stack_v1, allocation_low, 16);
PL_GNU_OFFSET(hermit_private_stack_v1, allocation_size, 24);
PL_GNU_OFFSET(hermit_private_stack_v1, usable_low, 32);
PL_GNU_OFFSET(hermit_private_stack_v1, usable_size, 40);
PL_GNU_OFFSET(hermit_private_stack_v1, lower_guard_size, 48);
PL_GNU_OFFSET(hermit_private_stack_v1, upper_guard_size, 56);
PL_GNU_OFFSET(hermit_private_stack_v1, reserved, 64);
_Static_assert(sizeof(struct hermit_private_stack_info_v1) == 104, "info ABI size");
PL_GNU_OFFSET(hermit_private_stack_info_v1, storage, 0);
PL_GNU_OFFSET(hermit_private_stack_info_v1, gnu_stack_low, 80);
PL_GNU_OFFSET(hermit_private_stack_info_v1, gnu_stack_size, 88);
PL_GNU_OFFSET(hermit_private_stack_info_v1, gnu_reported_guard_size, 96);
_Static_assert(sizeof(struct pl_gnu_result) == 16, "result ABI size");
_Static_assert(sizeof(struct hermit_private_reservation_owner_v1) == 40, "reservation owner size");
PL_GNU_OFFSET(hermit_private_reservation_owner_v1, version, 0);
PL_GNU_OFFSET(hermit_private_reservation_owner_v1, size, 4);
PL_GNU_OFFSET(hermit_private_reservation_owner_v1, invocation, 8);
PL_GNU_OFFSET(hermit_private_reservation_owner_v1, entry_generation, 16);
PL_GNU_OFFSET(hermit_private_reservation_owner_v1, reserved, 24);
_Static_assert(sizeof(struct hermit_private_reservation_v1) == 32, "reservation handle size");
PL_GNU_OFFSET(hermit_private_reservation_v1, version, 0);
PL_GNU_OFFSET(hermit_private_reservation_v1, size, 4);
PL_GNU_OFFSET(hermit_private_reservation_v1, ticket, 8);
PL_GNU_OFFSET(hermit_private_reservation_v1, reserved, 16);
PL_GNU_OFFSET(pl_gnu_result, stage, 0);
PL_GNU_OFFSET(pl_gnu_result, domain, 4);
PL_GNU_OFFSET(pl_gnu_result, code, 8);
PL_GNU_OFFSET(pl_gnu_result, reserved, 12);
#undef PL_GNU_OFFSET

__attribute__((visibility("hidden")))
struct pl_gnu_result pl_gnu_root(const struct pm_crt_context *context);
__attribute__((visibility("hidden")))
struct pl_gnu_result pl_gnu_check(const struct pm_crt_context *context);
__attribute__((visibility("hidden")))
struct pl_gnu_result pl_gnu_close(const struct pm_crt_context *context);
__attribute__((visibility("hidden")))
struct pl_gnu_result pl_gnu_prepare(const struct pm_crt_context *context,
    struct hermit_private_stack_v1 *stack, hermit_private_ticket *ticket);
__attribute__((visibility("hidden")))
struct pl_gnu_result pl_gnu_query(const struct pm_crt_context *context,
    hermit_private_ticket ticket, struct hermit_private_stack_info_v1 *out);
__attribute__((visibility("hidden")))
struct pl_gnu_result pl_gnu_rollback(const struct pm_crt_context *context,
    hermit_private_ticket ticket);
__attribute__((visibility("hidden")))
struct pl_gnu_result pl_gnu_reserve(const struct pm_crt_context *context,
    const struct hermit_private_reservation_owner_v1 *origin,
    struct hermit_private_stack_v1 *stack, struct hermit_private_reservation_v1 *out);
__attribute__((visibility("hidden")))
struct pl_gnu_result pl_gnu_reservation_query(const struct pm_crt_context *context,
    const struct hermit_private_reservation_owner_v1 *origin,
    const struct hermit_private_reservation_v1 *reservation,
    struct hermit_private_stack_info_v1 *out);
__attribute__((visibility("hidden")))
struct pl_gnu_result pl_gnu_cancel_reservation(const struct pm_crt_context *context,
    const struct hermit_private_reservation_owner_v1 *origin,
    const struct hermit_private_reservation_v1 *reservation);
#endif
