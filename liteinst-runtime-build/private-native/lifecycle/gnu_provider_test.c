#include <assert.h>
#include <stdio.h>
#include "private_tls.c"
#include "gnu_provider.c"

static struct pm_owner mapping;
static unsigned char stack_bytes[65536] __attribute__((aligned(4096)));
static uint64_t observed_tid, observed_fs, observed_gs;
static enum hermit_private_status domain_status, adopt_status, query_status, prepare_status, rollback_status;
static struct hermit_private_stack_v1 root_record, child_record;
static unsigned calls;
static int corrupt, ticket_on_failure, zero_ticket;
static struct hermit_private_reservation_owner_v1 reservation_origin_record;
static struct hermit_private_reservation_v1 reservation_record;
static int reservation_live, malformed_reservation;

long reverie_preload_trusted_syscall(long number, long first, long second,
    long third, long fourth, long fifth, long sixth) {
  (void)third; (void)fourth; (void)fifth; (void)sixth;
  if (number == __NR_gettid) return (long)observed_tid;
  assert(number == __NR_arch_prctl);
  assert(first == ARCH_GET_FS || first == ARCH_GET_GS);
  *(uint64_t *)(uintptr_t)second = first == ARCH_GET_FS ? observed_fs : observed_gs;
  return 0;
}

enum hermit_private_status __hermit_private_domain_prepare(void) {
  ++calls;
  return domain_status;
}
enum hermit_private_status __hermit_private_root_stack_adopt_v1(const struct hermit_private_stack_v1 *record) {
  ++calls;
  root_record = *record;
  return adopt_status;
}
static void info(const struct hermit_private_stack_v1 *record, struct hermit_private_stack_info_v1 *out) {
  *out = (struct hermit_private_stack_info_v1){*record, record->usable_low,
    record->usable_size, (size_t)corrupt};
}
enum hermit_private_status __hermit_private_root_stack_info_v1(struct hermit_private_stack_info_v1 *out, size_t size) {
  ++calls;
  assert(size == sizeof *out);
  info(&root_record, out);
  return query_status;
}
enum hermit_private_status __hermit_private_context_prepare_stack_v1(const struct hermit_private_stack_v1 *record, hermit_private_ticket *ticket) {
  ++calls;
  child_record = *record;
  if (!zero_ticket && (prepare_status == HERMIT_PRIVATE_OK || ticket_on_failure)) *ticket = 91;
  return prepare_status;
}
enum hermit_private_status __hermit_private_context_stack_info_v1(hermit_private_ticket ticket, struct hermit_private_stack_info_v1 *out, size_t size) {
  ++calls;
  assert(ticket == 91 && size == sizeof *out);
  info(&child_record, out);
  return query_status;
}
enum hermit_private_status __hermit_private_context_rollback(hermit_private_ticket ticket) {
  ++calls;
  assert(ticket == 91);
  return rollback_status;
}

enum hermit_private_status __hermit_private_context_reserve_v1(
    const struct hermit_private_reservation_owner_v1 *origin,
    const struct hermit_private_stack_v1 *record,
    struct hermit_private_reservation_v1 *out) {
  ++calls;
  child_record = *record;
  reservation_origin_record = *origin;
  if (!zero_ticket && (prepare_status == HERMIT_PRIVATE_OK || ticket_on_failure)) {
    reservation_record = (struct hermit_private_reservation_v1){
      .version = malformed_reservation ? 2 : 1, .size = sizeof *out, .ticket = 191};
    *out = reservation_record;
    reservation_live = 1;
  }
  return prepare_status;
}

static enum hermit_private_status mock_reservation_owner(
    const struct hermit_private_reservation_owner_v1 *origin,
    const struct hermit_private_reservation_v1 *reservation) {
  if (!reservation_live || memcmp(reservation, &reservation_record, sizeof *reservation) != 0)
    return HERMIT_PRIVATE_INVALID;
  if (memcmp(origin, &reservation_origin_record, sizeof *origin) != 0)
    return HERMIT_PRIVATE_WRONG_OWNER;
  return HERMIT_PRIVATE_OK;
}

enum hermit_private_status __hermit_private_context_reservation_info_v1(
    const struct hermit_private_reservation_owner_v1 *origin,
    const struct hermit_private_reservation_v1 *reservation,
    struct hermit_private_stack_info_v1 *out, size_t size) {
  ++calls;
  assert(size == sizeof *out);
  enum hermit_private_status status = mock_reservation_owner(origin, reservation);
  if (status != HERMIT_PRIVATE_OK) return status;
  info(&child_record, out);
  return query_status;
}

enum hermit_private_status __hermit_private_context_cancel_reservation_v1(
    const struct hermit_private_reservation_owner_v1 *origin,
    const struct hermit_private_reservation_v1 *reservation) {
  ++calls;
  enum hermit_private_status status = mock_reservation_owner(origin, reservation);
  if (status != HERMIT_PRIVATE_OK) return status;
  if (rollback_status == HERMIT_PRIVATE_OK) reservation_live = 0;
  return rollback_status;
}

static void reset(void) {
  memset(&mapping, 0, sizeof mapping);
  mapping.context.owner = &mapping;
  mapping.transferred = 1;
  mapping.status = PM_OK;
  mapping.stack = stack_bytes;
  mapping.stack_size = sizeof stack_bytes;
  private_owner = &mapping.context;
  atomic_store(&pl_tls.phase, 2);
  atomic_store(&pl_tls.deferred, 0);
  atomic_store(&pl_tls.owner_tid, 17);
  atomic_store(&pl_tls.private_fs, 0x1000);
  atomic_store(&pl_tls.private_gs, 0x2000);
  observed_tid = 17; observed_fs = 0x1000; observed_gs = 0x2000;
  atomic_store(&startup_phase, UNINITIALIZED);
  next_allocation_id = 1;
  domain_status = adopt_status = query_status = prepare_status = rollback_status = HERMIT_PRIVATE_OK;
  calls = 0;
  corrupt = ticket_on_failure = zero_ticket = 0;
  reservation_live = malformed_reservation = 0;
  memset(&reservation_record, 0, sizeof reservation_record);
  memset(&reservation_origin_record, 0, sizeof reservation_origin_record);
}

static struct hermit_private_stack_v1 child(void) {
  return (struct hermit_private_stack_v1){.version = 1, .size = 80,
    .allocation_low = 0x100000, .allocation_size = 65536, .usable_low = 0x100000,
    .usable_size = 65536};
}
static void success(struct pl_gnu_result result, enum pl_gnu_stage stage) {
  assert(result.stage == (uint32_t)stage && result.code == 0 && result.reserved == 0);
  assert(result.domain == 1 || result.domain == 2);
}

static void root_geometry(void) {
  success(pl_gnu_root(&mapping.context), PL_GNU_ROOT);
  struct hermit_private_stack_v1 expected = {.version = 1, .size = 80, .allocation_id = 1,
    .allocation_low = (uintptr_t)mapping.stack, .allocation_size = mapping.stack_size,
    .usable_low = (uintptr_t)mapping.stack, .usable_size = mapping.stack_size};
  assert(memcmp(&expected, &root_record, sizeof expected) == 0);
  struct hermit_private_stack_v1 record = child();
  hermit_private_ticket ticket = 0;
  success(pl_gnu_prepare(&mapping.context, &record, &ticket), PL_GNU_PREPARE);
  assert(ticket == 91 && record.allocation_id == 2 && next_allocation_id == 3);
  struct hermit_private_stack_info_v1 out;
  success(pl_gnu_query(&mapping.context, ticket, &out), PL_GNU_QUERY);
  assert(equal_info(&out, &record));
}

static void owner_gate(void) {
  assert(pl_gnu_root(NULL).code == PL_GNU_OWNER);
  struct pm_crt_context foreign = mapping.context;
  assert(pl_gnu_root(&foreign).code == PL_GNU_OWNER);
  observed_tid++;
  assert(pl_gnu_root(&mapping.context).code == PL_GNU_OWNER);
  observed_tid--;
  observed_fs++;
  assert(pl_gnu_root(&mapping.context).code == PL_GNU_OWNER);
  observed_fs--;
  observed_gs++;
  assert(pl_gnu_root(&mapping.context).code == PL_GNU_OWNER);
  observed_gs--;
  atomic_store(&pl_tls.deferred, 1);
  assert(pl_gnu_root(&mapping.context).code == PL_GNU_OWNER);
  assert(calls == 0);
}

static void phase_gate(void) {
  struct hermit_private_stack_v1 record = child(), before = record;
  hermit_private_ticket ticket = 0;
  assert(pl_gnu_prepare(&mapping.context, &record, &ticket).code == PL_GNU_PHASE);
  success(pl_gnu_root(&mapping.context), PL_GNU_ROOT);
  assert(pl_gnu_root(&mapping.context).code == PL_GNU_PHASE);
  unsigned previous = calls;
  atomic_store(&pl_tls.deferred, 1);
  assert(pl_gnu_prepare(&mapping.context, &record, &ticket).code == PL_GNU_OWNER);
  atomic_store(&pl_tls.deferred, 0);
  success(pl_gnu_close(&mapping.context), PL_GNU_CLOSE);
  assert(pl_gnu_check(&mapping.context).code == PL_GNU_PHASE);
  assert(pl_gnu_prepare(&mapping.context, &record, &ticket).code == PL_GNU_PHASE);
  assert(pl_gnu_rollback(&mapping.context, 91).code == PL_GNU_PHASE);
  assert(pl_gnu_root(&mapping.context).code == PL_GNU_PHASE);
  assert(pl_gnu_close(&mapping.context).code == PL_GNU_PHASE);
  assert(ticket == 0 && memcmp(&record, &before, sizeof record) == 0 && calls == previous);
}

static void identity_exhaustion(void) {
  success(pl_gnu_root(&mapping.context), PL_GNU_ROOT);
  struct hermit_private_stack_v1 record = child();
  hermit_private_ticket ticket = 0;
  next_allocation_id = UINT64_MAX;
  success(pl_gnu_prepare(&mapping.context, &record, &ticket), PL_GNU_PREPARE);
  assert(record.allocation_id == UINT64_MAX && next_allocation_id == 0);
  record = child();
  struct hermit_private_stack_v1 before = record;
  ticket = 0;
  unsigned previous = calls;
  assert(pl_gnu_prepare(&mapping.context, &record, &ticket).code == PL_GNU_EXHAUSTED);
  assert(ticket == 0 && memcmp(&record, &before, sizeof record) == 0 && calls == previous);
}

static void root_statuses(void) {
  for (unsigned stage = PL_GNU_DOMAIN; stage <= PL_GNU_ROOT_QUERY; ++stage) {
    for (unsigned code = 1; code <= 6; ++code) {
      reset();
      if (stage == PL_GNU_DOMAIN) domain_status = (enum hermit_private_status)code;
      if (stage == PL_GNU_ADOPT) adopt_status = (enum hermit_private_status)code;
      if (stage == PL_GNU_ROOT_QUERY) query_status = (enum hermit_private_status)code;
      struct pl_gnu_result result = pl_gnu_root(&mapping.context);
      assert(result.stage == stage && result.domain == 2 && result.code == code);
      assert(atomic_load(&startup_phase) == FAILED);
      assert(pl_gnu_root(&mapping.context).code == PL_GNU_PHASE);
    }
  }
}

static void prepare_statuses(void) {
  success(pl_gnu_root(&mapping.context), PL_GNU_ROOT);
  for (unsigned code = 1; code <= 6; ++code) {
    for (int writes_ticket = 0; writes_ticket <= 1; ++writes_ticket) {
      prepare_status = (enum hermit_private_status)code;
      ticket_on_failure = writes_ticket;
      struct hermit_private_stack_v1 record = child(), before = record;
      hermit_private_ticket ticket = 0;
      uint64_t previous_id = next_allocation_id;
      struct pl_gnu_result result = pl_gnu_prepare(&mapping.context, &record, &ticket);
      assert(result.stage == PL_GNU_PREPARE && result.domain == 2 && result.code == code);
      assert(next_allocation_id == previous_id + 1);
      if (writes_ticket) assert(ticket == 91 && record.allocation_id == previous_id);
      else assert(ticket == 0 && memcmp(&record, &before, sizeof record) == 0);
    }
  }
  zero_ticket = 1;
  prepare_status = HERMIT_PRIVATE_OK;
  struct hermit_private_stack_v1 record = child();
  hermit_private_ticket ticket = 0;
  assert(pl_gnu_prepare(&mapping.context, &record, &ticket).code == PL_GNU_MISMATCH);
}

static void query_unchanged(void) {
  root_geometry();
  for (unsigned code = 1; code <= 6; ++code) {
    query_status = (enum hermit_private_status)code;
    struct hermit_private_stack_info_v1 out, before;
    memset(&out, 0xa5, sizeof out); before = out;
    struct pl_gnu_result result = pl_gnu_query(&mapping.context, 91, &out);
    assert(result.stage == PL_GNU_QUERY && result.domain == 2 && result.code == code);
    assert(memcmp(&out, &before, sizeof out) == 0);
  }
}

static void rollback_statuses(void) {
  root_geometry();
  for (unsigned code = 0; code <= 6; ++code) {
    rollback_status = (enum hermit_private_status)code;
    struct pl_gnu_result result = pl_gnu_rollback(&mapping.context, 91);
    assert(result.stage == PL_GNU_ROLLBACK && result.domain == 2 && result.code == code);
  }
}

static void root_mismatch(void) {
  corrupt = 1;
  struct pl_gnu_result result = pl_gnu_root(&mapping.context);
  assert(result.stage == PL_GNU_ROOT_QUERY && result.domain == 1 && result.code == PL_GNU_MISMATCH);
  assert(atomic_load(&startup_phase) == FAILED);
}

static void bad_inputs(void) {
  mapping.stack_size = SIZE_MAX;
  assert(pl_gnu_root(&mapping.context).code == PL_GNU_INVALID);
  assert(calls == 0);
  reset();
  success(pl_gnu_root(&mapping.context), PL_GNU_ROOT);
  struct hermit_private_stack_v1 record = child(), before = record;
  hermit_private_ticket ticket = 1;
  unsigned previous = calls;
  assert(pl_gnu_prepare(&mapping.context, &record, &ticket).code == PL_GNU_INVALID);
  assert(ticket == 1 && memcmp(&record, &before, sizeof record) == 0);
  assert(pl_gnu_query(&mapping.context, 91, NULL).code == PL_GNU_INVALID);
  assert(pl_gnu_rollback(&mapping.context, 0).code == PL_GNU_INVALID);
  assert(calls == previous);
}

static struct hermit_private_reservation_owner_v1 new_origin(void) {
  return (struct hermit_private_reservation_owner_v1){
    .version = 1, .size = sizeof(struct hermit_private_reservation_owner_v1),
    .invocation = 23, .entry_generation = 9};
}

static void close_startup(void) {
  success(pl_gnu_root(&mapping.context), PL_GNU_ROOT);
  success(pl_gnu_close(&mapping.context), PL_GNU_CLOSE);
  assert(atomic_load(&startup_phase) == CLOSED);
}

static void reservation_phase(void) {
  struct hermit_private_reservation_owner_v1 origin = new_origin();
  for (unsigned phase = UNINITIALIZED; phase <= FAILED; ++phase) {
    if (phase == CLOSED) continue;
    reset();
    atomic_store(&startup_phase, phase);
    struct hermit_private_stack_v1 record = child(), before = record;
    struct hermit_private_reservation_v1 reservation = {0};
    struct hermit_private_stack_info_v1 output;
    memset(&output, 0xa5, sizeof output);
    struct hermit_private_stack_info_v1 sentinel = output;
    assert(pl_gnu_reserve(&mapping.context, &origin, &record, &reservation).code == PL_GNU_PHASE);
    assert(pl_gnu_reservation_query(&mapping.context, &origin, &reservation, &output).code == PL_GNU_PHASE);
    assert(pl_gnu_cancel_reservation(&mapping.context, &origin, &reservation).code == PL_GNU_PHASE);
    assert(calls == 0 && reservation.ticket == 0 && next_allocation_id == 1);
    assert(memcmp(&record, &before, sizeof record) == 0);
    assert(memcmp(&output, &sentinel, sizeof output) == 0);
    assert(atomic_load(&startup_phase) == phase);
  }
  reset(); close_startup();
  struct hermit_private_stack_v1 record = child();
  struct hermit_private_reservation_v1 reservation = {0};
  success(pl_gnu_reserve(&mapping.context, &origin, &record, &reservation), PL_GNU_RESERVE);
  assert(reservation.ticket == 191 && record.allocation_id == 2);
  assert(atomic_load(&startup_phase) == CLOSED);
  assert(pl_gnu_close(&mapping.context).code == PL_GNU_PHASE);
  assert(reservation_live);
  assert(pl_gnu_root(&mapping.context).code == PL_GNU_PHASE);
  assert(pl_gnu_check(&mapping.context).code == PL_GNU_PHASE);
  hermit_private_ticket legacy = 0;
  assert(pl_gnu_prepare(&mapping.context, &record, &legacy).code == PL_GNU_PHASE);
  struct hermit_private_stack_info_v1 output;
  assert(pl_gnu_query(&mapping.context, 91, &output).code == PL_GNU_PHASE);
  assert(pl_gnu_rollback(&mapping.context, 91).code == PL_GNU_PHASE);
}

static void reservation_owner(void) {
  close_startup();
  struct hermit_private_reservation_owner_v1 origin = new_origin();
  for (unsigned wrong = 0; wrong != 5; ++wrong) {
    struct hermit_private_stack_v1 record = child(), before = record;
    struct hermit_private_reservation_v1 reservation = {0};
    struct pm_crt_context foreign = mapping.context;
    const struct pm_crt_context *context = &mapping.context;
    if (wrong == 0) context = &foreign;
    if (wrong == 1) ++observed_tid;
    if (wrong == 2) ++observed_fs;
    if (wrong == 3) ++observed_gs;
    if (wrong == 4) atomic_store(&pl_tls.deferred, 1);
    unsigned previous = calls;
    assert(pl_gnu_reserve(context, &origin, &record, &reservation).code == PL_GNU_OWNER);
    assert(pl_gnu_reservation_query(context, &origin, &reservation, NULL).code == PL_GNU_OWNER);
    assert(pl_gnu_cancel_reservation(context, &origin, &reservation).code == PL_GNU_OWNER);
    assert(calls == previous && reservation.ticket == 0 && next_allocation_id == 2);
    assert(memcmp(&record, &before, sizeof record) == 0);
    observed_tid = 17; observed_fs = 0x1000; observed_gs = 0x2000;
    atomic_store(&pl_tls.deferred, 0);
  }
}

static void reservation_lifetime(void) {
  close_startup();
  struct hermit_private_reservation_owner_v1 origin = new_origin(), other = origin;
  struct hermit_private_stack_v1 record = child();
  struct hermit_private_reservation_v1 reservation = {0};
  success(pl_gnu_reserve(&mapping.context, &origin, &record, &reservation), PL_GNU_RESERVE);
  struct hermit_private_reservation_v1 copied = reservation;
  struct hermit_private_stack_info_v1 output;
  success(pl_gnu_reservation_query(&mapping.context, &origin, &reservation, &output), PL_GNU_RESERVATION_QUERY);
  assert(equal_info(&output, &record));
  ++other.invocation;
  assert(pl_gnu_cancel_reservation(&mapping.context, &other, &reservation).code == HERMIT_PRIVATE_WRONG_OWNER);
  other = origin; ++other.entry_generation;
  assert(pl_gnu_cancel_reservation(&mapping.context, &other, &reservation).code == HERMIT_PRIVATE_WRONG_OWNER);
  assert(reservation_live);
  success(pl_gnu_cancel_reservation(&mapping.context, &origin, &copied), PL_GNU_CANCEL_RESERVATION);
  assert(!reservation_live);
  struct pl_gnu_result result = pl_gnu_cancel_reservation(&mapping.context, &origin, &reservation);
  assert(result.domain == 2 && result.code == HERMIT_PRIVATE_INVALID);
  assert(atomic_load(&startup_phase) == CLOSED);
}

static void reservation_statuses(void) {
  struct hermit_private_reservation_owner_v1 origin = new_origin();
  for (unsigned code = 1; code <= 6; ++code) {
    for (int writes = 0; writes != 2; ++writes) {
      reset(); close_startup();
      prepare_status = (enum hermit_private_status)code;
      ticket_on_failure = writes;
      struct hermit_private_stack_v1 record = child(), before = record;
      struct hermit_private_reservation_v1 reservation = {0};
      struct pl_gnu_result result = pl_gnu_reserve(&mapping.context, &origin, &record, &reservation);
      assert(result.stage == PL_GNU_RESERVE && result.domain == 2 && result.code == code);
      assert(next_allocation_id == 3 && atomic_load(&startup_phase) == CLOSED);
      if (writes) assert(reservation.ticket == 191 && record.allocation_id == 2);
      else assert(reservation.ticket == 0 && memcmp(&record, &before, sizeof record) == 0);
    }
  }
  for (unsigned kind = 0; kind != 2; ++kind) {
    reset(); close_startup();
    zero_ticket = kind == 0;
    malformed_reservation = kind == 1;
    struct hermit_private_stack_v1 record = child();
    struct hermit_private_reservation_v1 reservation = {0};
    struct pl_gnu_result result = pl_gnu_reserve(&mapping.context, &origin, &record, &reservation);
    assert(result.domain == 1 && result.code == PL_GNU_MISMATCH);
    assert(reservation.ticket == (kind == 0 ? 0 : 191));
  }
  reset(); close_startup();
  struct hermit_private_stack_v1 record = child();
  struct hermit_private_reservation_v1 reservation = {0};
  success(pl_gnu_reserve(&mapping.context, &origin, &record, &reservation), PL_GNU_RESERVE);
  for (unsigned code = 1; code <= 6; ++code) {
    query_status = rollback_status = (enum hermit_private_status)code;
    struct hermit_private_stack_info_v1 output;
    memset(&output, 0xa5, sizeof output);
    struct hermit_private_stack_info_v1 before = output;
    struct pl_gnu_result result = pl_gnu_reservation_query(&mapping.context, &origin, &reservation, &output);
    assert(result.stage == PL_GNU_RESERVATION_QUERY && result.domain == 2 && result.code == code);
    assert(memcmp(&output, &before, sizeof output) == 0);
    result = pl_gnu_cancel_reservation(&mapping.context, &origin, &reservation);
    assert(result.stage == PL_GNU_CANCEL_RESERVATION && result.domain == 2 && result.code == code);
    assert(reservation_live);
  }
}

static void reservation_bad_inputs(void) {
  close_startup();
  struct hermit_private_reservation_owner_v1 origin = new_origin();
  for (unsigned field = 0; field != 6; ++field) {
    struct hermit_private_reservation_owner_v1 bad = origin;
    if (field == 0) bad.version = 2;
    if (field == 1) --bad.size;
    if (field == 2) bad.invocation = 0;
    if (field == 3) bad.entry_generation = 0;
    if (field == 4) bad.reserved[0] = 1;
    if (field == 5) bad.reserved[1] = 1;
    struct hermit_private_stack_v1 record = child();
    struct hermit_private_reservation_v1 reservation = {0};
    unsigned before = calls;
    assert(pl_gnu_reserve(&mapping.context, &bad, &record, &reservation).code == PL_GNU_INVALID);
    assert(pl_gnu_reservation_query(&mapping.context, &bad, &reservation, NULL).code == PL_GNU_INVALID);
    assert(pl_gnu_cancel_reservation(&mapping.context, &bad, &reservation).code == PL_GNU_INVALID);
    assert(calls == before && next_allocation_id == 2 && reservation.ticket == 0);
  }
  struct hermit_private_stack_v1 record = child();
  struct hermit_private_reservation_v1 reservation = {0};
  next_allocation_id = UINT64_MAX;
  success(pl_gnu_reserve(&mapping.context, &origin, &record, &reservation), PL_GNU_RESERVE);
  assert(record.allocation_id == UINT64_MAX && next_allocation_id == 0);
  struct hermit_private_reservation_v1 empty = {0};
  record = child();
  assert(pl_gnu_reserve(&mapping.context, &origin, &record, &empty).code == PL_GNU_EXHAUSTED);
  assert(empty.ticket == 0 && record.allocation_id == 0);
  for (unsigned field = 0; field != 5; ++field) {
    struct hermit_private_reservation_v1 bad = reservation;
    if (field == 0) bad.version = 2;
    if (field == 1) --bad.size;
    if (field == 2) bad.ticket = 0;
    if (field == 3) bad.reserved[0] = 1;
    if (field == 4) bad.reserved[1] = 1;
    unsigned before = calls;
    struct hermit_private_stack_info_v1 output;
    assert(pl_gnu_reservation_query(&mapping.context, &origin, &bad, &output).code == PL_GNU_INVALID);
    assert(pl_gnu_cancel_reservation(&mapping.context, &origin, &bad).code == PL_GNU_INVALID);
    assert(calls == before);
  }
}

int main(int argc, char **argv) {
  assert(argc == 2);
  const struct { const char *name; void (*run)(void); } controls[] = {
    {"root_geometry", root_geometry}, {"owner_gate", owner_gate}, {"phase_gate", phase_gate},
    {"identity_exhaustion", identity_exhaustion}, {"root_statuses", root_statuses},
    {"prepare_statuses", prepare_statuses}, {"query_unchanged", query_unchanged},
    {"rollback_statuses", rollback_statuses}, {"root_mismatch", root_mismatch}, {"bad_inputs", bad_inputs},
    {"reservation_phase", reservation_phase}, {"reservation_owner", reservation_owner},
    {"reservation_lifetime", reservation_lifetime}, {"reservation_statuses", reservation_statuses},
    {"reservation_bad_inputs", reservation_bad_inputs}
  };
  for (size_t index = 0; index < sizeof controls / sizeof *controls; ++index) {
    if (strcmp(argv[1], controls[index].name) == 0) {
      reset(); controls[index].run(); puts(controls[index].name); return 0;
    }
  }
  return 2;
}
