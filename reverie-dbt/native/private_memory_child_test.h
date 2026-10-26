/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the root LICENSE file. */

/* Explicit live-test observations, never ownership-provider inputs. Each
 * snapshot comes from the actual runtime query. Only these records/barriers
 * are shared; no test replaces the installed retirement observer. */
enum {
  CHILD_TEST_MAGIC,
  CHILD_TEST_BEFORE_READY,
  CHILD_TEST_SETUP_READY,
  CHILD_TEST_AFTER_READY,
  CHILD_TEST_BEFORE_ELIGIBLE,
  CHILD_TEST_SETUP_ELIGIBLE,
  CHILD_TEST_AFTER_ELIGIBLE,
  CHILD_TEST_REBOUND_READY,
  CHILD_TEST_RETIRED_READY,
  CHILD_TEST_PARENT_READY,
  CHILD_TEST_WAITING,
  CHILD_TEST_RELEASE,
  CHILD_TEST_CHECKED,
  CHILD_TEST_FAILED,
  CHILD_TEST_PARENT_PID,
  CHILD_TEST_CHILD_PID,
  CHILD_TEST_WORDS
};
typedef struct {
  _Atomic uint64_t words[CHILD_TEST_WORDS];
  private_memory_snapshot_t snapshots[6];
} private_memory_child_test_t;
static private_memory_child_test_t* private_memory_child_test;

static void private_memory_child_test_require(bool ok, const char* name) {
  if (!ok) {
    atomic_store(&private_memory_child_test->words[CHILD_TEST_FAILED], 1);
    atomic_store(&private_memory_child_test->words[CHILD_TEST_RELEASE], 1);
    private_memory_test_check(false, name);
  }
}

static uint64_t private_memory_child_test_eligible(void) {
  return mapping_capture_slot == NULL ? 2 :
      (uint64_t)atomic_load_explicit(&mapping_capture_slot->eligible, memory_order_acquire);
}

static void private_memory_child_test_initialize(void) {
  if (test_private_memory_child_mode == 0)
    return;
  file_t file = dr_open_file(test_private_memory_child_path,
      DR_FILE_READ | DR_FILE_WRITE_APPEND);
  DR_ASSERT(file != INVALID_FILE);
  size_t size = 4096;
  private_memory_child_test = dr_map_file(file, &size, 0, NULL,
      DR_MEMPROT_READ | DR_MEMPROT_WRITE, 0);
  DR_ASSERT(private_memory_child_test != NULL && size >= sizeof(*private_memory_child_test));
  dr_close_file(file);
  memset(private_memory_child_test, 0, sizeof(*private_memory_child_test));
  atomic_store(&private_memory_child_test->words[CHILD_TEST_MAGIC],
      UINT64_C(0x4348494c444f574e));
  atomic_store(&private_memory_child_test->words[CHILD_TEST_PARENT_PID], dr_get_process_id());
}

static bool private_memory_child_test_is_parent(void) {
  return private_memory_child_test != NULL &&
      (uint64_t)dr_get_process_id() ==
          atomic_load(&private_memory_child_test->words[CHILD_TEST_PARENT_PID]);
}

static void private_memory_child_test_before_birth(void) {
  if (!private_memory_child_test_is_parent() ||
      atomic_load(&private_memory_child_test->words[CHILD_TEST_BEFORE_READY]) != 0)
    return;
  private_memory_child_test_t* r = private_memory_child_test;
  int ready = runtime_private_memory(0, NULL, &r->snapshots[0]);
  atomic_store(&r->words[CHILD_TEST_BEFORE_READY], ready);
  atomic_store(&r->words[CHILD_TEST_BEFORE_ELIGIBLE], private_memory_child_test_eligible());
  private_memory_child_test_require(ready == 1, "parent-private-before-birth-not-ready");
}

static void private_memory_child_test_parent_setup(void) {
  if (!private_memory_child_test_is_parent())
    return;
  private_memory_child_test_t* r = private_memory_child_test;
  while (atomic_load_explicit(&r->words[CHILD_TEST_WAITING], memory_order_acquire) == 0)
    __asm__ volatile("pause" ::: "memory");
  int ready = runtime_private_memory(0, NULL, &r->snapshots[1]);
  atomic_store(&r->words[CHILD_TEST_SETUP_READY], ready);
  atomic_store(&r->words[CHILD_TEST_SETUP_ELIGIBLE], private_memory_child_test_eligible());
  private_memory_child_test_require(ready == 1, "parent-private-after-setup-not-ready");
  atomic_store_explicit(&r->words[CHILD_TEST_RELEASE], 1, memory_order_release);
}

static void private_memory_child_test_barrier(void) {
  private_memory_child_test_t* r = private_memory_child_test;
  atomic_store(&r->words[CHILD_TEST_CHILD_PID], dr_get_process_id());
  atomic_store_explicit(&r->words[CHILD_TEST_WAITING], 1, memory_order_release);
  while (atomic_load_explicit(&r->words[CHILD_TEST_RELEASE], memory_order_acquire) == 0)
    __asm__ volatile("pause" ::: "memory");
  private_memory_child_test_require(atomic_load(&r->words[CHILD_TEST_FAILED]) == 0,
      "parent-private-prerequisite-failed");
}

static void private_memory_child_test_refused(void) {
  if (test_private_memory_child_mode != 1 || private_memory_child_test == NULL)
    return;
  private_memory_child_test_barrier();
  atomic_store(&private_memory_child_test->words[CHILD_TEST_CHECKED], 1);
}

static void private_memory_child_test_rebound(void) {
  if (test_private_memory_child_mode != 2 || private_memory_child_test == NULL)
    return;
  private_memory_child_test_t* r = private_memory_child_test;
  private_memory_child_test_barrier();
  int ready = runtime_private_memory(0, NULL, &r->snapshots[3]);
  atomic_store(&r->words[CHILD_TEST_REBOUND_READY], ready);
  private_memory_child_test_require(ready == 1 &&
      r->snapshots[3].owner[2] != r->snapshots[1].owner[2], "child-private-rekey-not-ready");
  /* Actual bootstrap code is an independent owned-address witness. This seam
   * invokes the real normalized retirement path without unmapping live DR. */
  reg_t args[6] = {(reg_t)(ptr_uint_t)dr_get_private_reservations, 1, 0, 0, 0, 0};
  dr_test_private_reservation_syscall(SYS_munmap, args);
  ready = runtime_private_memory(0, NULL, &r->snapshots[4]);
  atomic_store(&r->words[CHILD_TEST_RETIRED_READY], ready);
  const private_memory_snapshot_t zero = {0};
  private_memory_child_test_require(ready == 0 &&
      memcmp(&r->snapshots[4], &zero, sizeof(zero)) == 0 &&
      private_memory_test_unavailable(&r->snapshots[3]), "child-observer-not-reinstalled");
  ready = runtime_private_memory(0, &r->snapshots[1], &r->snapshots[5]);
  atomic_store(&r->words[CHILD_TEST_PARENT_READY], ready);
  private_memory_child_test_require(ready == 1 &&
      memcmp(&r->snapshots[1], &r->snapshots[5], sizeof(zero)) == 0,
      "child-observer-retired-parent");
  atomic_store_explicit(&r->words[CHILD_TEST_CHECKED], 1, memory_order_release);
}

static void private_memory_child_test_waited(void) {
  if (!private_memory_child_test_is_parent())
    return;
  private_memory_child_test_t* r = private_memory_child_test;
  int ready = runtime_private_memory(0, &r->snapshots[1], &r->snapshots[2]);
  atomic_store(&r->words[CHILD_TEST_AFTER_READY], ready);
  atomic_store(&r->words[CHILD_TEST_AFTER_ELIGIBLE], private_memory_child_test_eligible());
  private_memory_child_test_require(ready == 1 &&
      memcmp(&r->snapshots[1], &r->snapshots[2], sizeof(r->snapshots[1])) == 0,
      "child-exit-retired-parent");
  private_memory_child_test_require(
      atomic_load_explicit(&r->words[CHILD_TEST_CHECKED], memory_order_acquire) == 1,
      "child-observer-check-not-completed");
  dr_fprintf(diagnostic_file,
      "PRIVATE_MEMORY_CHILD mode=%d before_ready=1 setup_ready=1 after_ready=1 "
      "eligible=%llu,%llu,%llu before_version=%llu setup_version=%llu after_version=%llu\n",
      test_private_memory_child_mode,
      (unsigned long long)atomic_load(&r->words[CHILD_TEST_BEFORE_ELIGIBLE]),
      (unsigned long long)atomic_load(&r->words[CHILD_TEST_SETUP_ELIGIBLE]),
      (unsigned long long)atomic_load(&r->words[CHILD_TEST_AFTER_ELIGIBLE]),
      (unsigned long long)r->snapshots[0].version,
      (unsigned long long)r->snapshots[1].version,
      (unsigned long long)r->snapshots[2].version);
}
