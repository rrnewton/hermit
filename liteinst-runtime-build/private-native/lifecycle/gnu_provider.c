#include "gnu_provider.h"
#include "mapper.h"
#include <string.h>

enum startup_phase { UNINITIALIZED, BUSY, DOMAIN_PREPARED, ROOT_ADOPTED, CLOSED, FAILED };
static _Atomic unsigned startup_phase;
static _Atomic uint64_t next_allocation_id = 1;

static struct pl_gnu_result local(enum pl_gnu_stage stage, enum pl_gnu_error code) {
  return (struct pl_gnu_result){stage, 1, code, 0};
}

static struct pl_gnu_result provider(enum pl_gnu_stage stage, enum hermit_private_status code) {
  return (struct pl_gnu_result){stage, 2, (uint32_t)code, 0};
}

static int owner(const struct pm_crt_context *context) {
  return pl_is_private_owner(context) == 0 &&
    atomic_load_explicit(&pl_tls.deferred, memory_order_acquire) == 0;
}

static struct pl_gnu_result enter(const struct pm_crt_context *context,
    enum pl_gnu_stage stage, unsigned from) {
  if (!owner(context)) return local(stage, PL_GNU_OWNER);
  unsigned expected = from;
  if (!atomic_compare_exchange_strong_explicit(&startup_phase, &expected, BUSY,
      memory_order_acq_rel, memory_order_acquire)) return local(stage, PL_GNU_PHASE);
  return local(stage, PL_GNU_OK);
}

static int issue_id(uint64_t *out) {
  uint64_t current = atomic_load_explicit(&next_allocation_id, memory_order_relaxed);
  while (current != 0) {
    uint64_t next = current == UINT64_MAX ? 0 : current + 1;
    if (atomic_compare_exchange_weak_explicit(&next_allocation_id, &current, next,
        memory_order_relaxed, memory_order_relaxed)) {
      *out = current;
      return 1;
    }
  }
  return 0;
}

static int equal_info(const struct hermit_private_stack_info_v1 *info,
    const struct hermit_private_stack_v1 *stack) {
  return memcmp(&info->storage, stack, sizeof *stack) == 0 &&
    info->gnu_stack_low == stack->usable_low && info->gnu_stack_size == stack->usable_size &&
    info->gnu_reported_guard_size == 0;
}

struct pl_gnu_result pl_gnu_root(const struct pm_crt_context *context) {
  struct pl_gnu_result result = enter(context, PL_GNU_ROOT, UNINITIALIZED);
  if (result.code != 0) return result;
  atomic_store_explicit(&startup_phase, FAILED, memory_order_release);
  const struct pm_owner *mapping = context->owner;
  if (mapping == NULL || context != &mapping->context || mapping->transferred != 1 ||
      mapping->status != PM_OK || mapping->stack == NULL || mapping->stack_size == 0 ||
      mapping->stack_size > UINTPTR_MAX - (uintptr_t)mapping->stack)
    return local(PL_GNU_ROOT, PL_GNU_INVALID);
  struct hermit_private_stack_v1 stack = {
    .version = 1, .size = sizeof stack,
    .allocation_low = (uintptr_t)mapping->stack, .allocation_size = mapping->stack_size,
    .usable_low = (uintptr_t)mapping->stack, .usable_size = mapping->stack_size
  };
  if (!issue_id(&stack.allocation_id)) return local(PL_GNU_ROOT, PL_GNU_EXHAUSTED);
  enum hermit_private_status status = __hermit_private_domain_prepare();
  if (status != HERMIT_PRIVATE_OK) return provider(PL_GNU_DOMAIN, status);
  atomic_store_explicit(&startup_phase, DOMAIN_PREPARED, memory_order_release);
  status = __hermit_private_root_stack_adopt_v1(&stack);
  if (status != HERMIT_PRIVATE_OK) {
    atomic_store_explicit(&startup_phase, FAILED, memory_order_release);
    return provider(PL_GNU_ADOPT, status);
  }
  struct hermit_private_stack_info_v1 info = {0};
  status = __hermit_private_root_stack_info_v1(&info, sizeof info);
  atomic_store_explicit(&startup_phase, FAILED, memory_order_release);
  if (status != HERMIT_PRIVATE_OK) return provider(PL_GNU_ROOT_QUERY, status);
  if (!equal_info(&info, &stack)) return local(PL_GNU_ROOT_QUERY, PL_GNU_MISMATCH);
  atomic_store_explicit(&startup_phase, ROOT_ADOPTED, memory_order_release);
  return local(PL_GNU_ROOT, PL_GNU_OK);
}

struct pl_gnu_result pl_gnu_check(const struct pm_crt_context *context) {
  struct pl_gnu_result result = enter(context, PL_GNU_CHECK, ROOT_ADOPTED);
  if (result.code == 0) atomic_store_explicit(&startup_phase, ROOT_ADOPTED, memory_order_release);
  return result;
}

struct pl_gnu_result pl_gnu_close(const struct pm_crt_context *context) {
  struct pl_gnu_result result = enter(context, PL_GNU_CLOSE, ROOT_ADOPTED);
  if (result.code == 0) atomic_store_explicit(&startup_phase, CLOSED, memory_order_release);
  return result;
}

struct pl_gnu_result pl_gnu_prepare(const struct pm_crt_context *context,
    struct hermit_private_stack_v1 *stack, hermit_private_ticket *ticket) {
  struct pl_gnu_result result = enter(context, PL_GNU_PREPARE, ROOT_ADOPTED);
  if (result.code != 0) return result;
  result = local(PL_GNU_PREPARE, PL_GNU_INVALID);
  if (stack != NULL && ticket != NULL && *ticket == 0 && stack->allocation_id == 0) {
    struct hermit_private_stack_v1 record = *stack;
    if (!issue_id(&record.allocation_id)) result = local(PL_GNU_PREPARE, PL_GNU_EXHAUSTED);
    else {
      hermit_private_ticket prepared = 0;
      enum hermit_private_status status = __hermit_private_context_prepare_stack_v1(&record, &prepared);
      if (prepared != 0) {
        *ticket = prepared;
        *stack = record;
      }
      result = provider(PL_GNU_PREPARE, status);
      if (status == HERMIT_PRIVATE_OK && prepared == 0)
        result = local(PL_GNU_PREPARE, PL_GNU_MISMATCH);
    }
  }
  atomic_store_explicit(&startup_phase, ROOT_ADOPTED, memory_order_release);
  return result;
}

struct pl_gnu_result pl_gnu_query(const struct pm_crt_context *context,
    hermit_private_ticket ticket, struct hermit_private_stack_info_v1 *out) {
  struct pl_gnu_result result = enter(context, PL_GNU_QUERY, ROOT_ADOPTED);
  if (result.code != 0) return result;
  result = local(PL_GNU_QUERY, PL_GNU_INVALID);
  if (ticket != 0 && out != NULL) {
    struct hermit_private_stack_info_v1 info = {0};
    enum hermit_private_status status = __hermit_private_context_stack_info_v1(ticket, &info, sizeof info);
    if (status == HERMIT_PRIVATE_OK) *out = info;
    result = provider(PL_GNU_QUERY, status);
  }
  atomic_store_explicit(&startup_phase, ROOT_ADOPTED, memory_order_release);
  return result;
}

struct pl_gnu_result pl_gnu_rollback(const struct pm_crt_context *context,
    hermit_private_ticket ticket) {
  struct pl_gnu_result result = enter(context, PL_GNU_ROLLBACK, ROOT_ADOPTED);
  if (result.code != 0) return result;
  result = ticket == 0 ? local(PL_GNU_ROLLBACK, PL_GNU_INVALID) :
    provider(PL_GNU_ROLLBACK, __hermit_private_context_rollback(ticket));
  atomic_store_explicit(&startup_phase, ROOT_ADOPTED, memory_order_release);
  return result;
}

static struct pl_gnu_result reservation_enter(const struct pm_crt_context *context,
    enum pl_gnu_stage stage, const struct hermit_private_reservation_owner_v1 *origin) {
  if (!owner(context)) return local(stage, PL_GNU_OWNER);
  if (atomic_load_explicit(&startup_phase, memory_order_acquire) != CLOSED)
    return local(stage, PL_GNU_PHASE);
  if (origin == NULL || origin->version != 1 || origin->size != sizeof *origin ||
      origin->invocation == 0 || origin->entry_generation == 0 ||
      origin->reserved[0] != 0 || origin->reserved[1] != 0)
    return local(stage, PL_GNU_INVALID);
  return local(stage, PL_GNU_OK);
}

struct pl_gnu_result pl_gnu_child_authorize(const struct pm_crt_context *context,
    const struct hermit_private_reservation_owner_v1 *origin) {
  return reservation_enter(context, PL_GNU_CHILD_AUTHORIZE, origin);
}

struct pl_gnu_result pl_gnu_child_poison(const struct pm_crt_context *context,
    const struct hermit_private_reservation_owner_v1 *origin) {
  struct pl_gnu_result result = reservation_enter(context, PL_GNU_CHILD_POISON, origin);
  if (result.code != 0) return result;
  unsigned expected = CLOSED;
  if (!atomic_compare_exchange_strong_explicit(&startup_phase, &expected, FAILED,
      memory_order_acq_rel, memory_order_acquire))
    return local(PL_GNU_CHILD_POISON, PL_GNU_PHASE);
  return local(PL_GNU_CHILD_POISON, PL_GNU_OK);
}

static int valid_reservation(const struct hermit_private_reservation_v1 *reservation) {
  return reservation != NULL && reservation->version == 1 &&
    reservation->size == sizeof *reservation && reservation->ticket != 0 &&
    reservation->reserved[0] == 0 && reservation->reserved[1] == 0;
}

struct pl_gnu_result pl_gnu_reserve(const struct pm_crt_context *context,
    const struct hermit_private_reservation_owner_v1 *origin,
    struct hermit_private_stack_v1 *stack, struct hermit_private_reservation_v1 *out) {
  struct pl_gnu_result result = reservation_enter(context, PL_GNU_RESERVE, origin);
  if (result.code != 0) return result;
  const struct hermit_private_reservation_v1 empty = {0};
  if (stack == NULL || out == NULL || stack->allocation_id != 0 ||
      memcmp(out, &empty, sizeof empty) != 0)
    return local(PL_GNU_RESERVE, PL_GNU_INVALID);
  struct hermit_private_stack_v1 record = *stack;
  if (!issue_id(&record.allocation_id)) return local(PL_GNU_RESERVE, PL_GNU_EXHAUSTED);
  struct hermit_private_reservation_v1 reservation = {0};
  enum hermit_private_status status = __hermit_private_context_reserve_v1(origin, &record, &reservation);
  if (reservation.ticket != 0) {
    *out = reservation;
    *stack = record;
  }
  if (status == HERMIT_PRIVATE_OK && !valid_reservation(&reservation))
    return local(PL_GNU_RESERVE, PL_GNU_MISMATCH);
  return provider(PL_GNU_RESERVE, status);
}

struct pl_gnu_result pl_gnu_reservation_query(const struct pm_crt_context *context,
    const struct hermit_private_reservation_owner_v1 *origin,
    const struct hermit_private_reservation_v1 *reservation,
    struct hermit_private_stack_info_v1 *out) {
  struct pl_gnu_result result = reservation_enter(context, PL_GNU_RESERVATION_QUERY, origin);
  if (result.code != 0) return result;
  if (!valid_reservation(reservation) || out == NULL)
    return local(PL_GNU_RESERVATION_QUERY, PL_GNU_INVALID);
  struct hermit_private_stack_info_v1 info = {0};
  enum hermit_private_status status = __hermit_private_context_reservation_info_v1(
      origin, reservation, &info, sizeof info);
  if (status == HERMIT_PRIVATE_OK) *out = info;
  return provider(PL_GNU_RESERVATION_QUERY, status);
}

struct pl_gnu_result pl_gnu_cancel_reservation(const struct pm_crt_context *context,
    const struct hermit_private_reservation_owner_v1 *origin,
    const struct hermit_private_reservation_v1 *reservation) {
  struct pl_gnu_result result = reservation_enter(context, PL_GNU_CANCEL_RESERVATION, origin);
  if (result.code != 0) return result;
  if (!valid_reservation(reservation)) return local(PL_GNU_CANCEL_RESERVATION, PL_GNU_INVALID);
  return provider(PL_GNU_CANCEL_RESERVATION,
      __hermit_private_context_cancel_reservation_v1(origin, reservation));
}
