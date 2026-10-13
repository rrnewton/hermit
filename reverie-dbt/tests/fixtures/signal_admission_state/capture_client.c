#define _GNU_SOURCE
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include "dr_api.h"
#include "drmgr.h"

typedef struct {
  uint64 admission_token;
  unsigned phase;
  bool inside_callback;
} state_t;
static int tls_index, original_pid;
static char mode[32] = "invalid";
static byte *stub;
static const int *parent_address, *children_address, *release_address, *stage_address;
static const uint64_t *mask_address;
static _Atomic unsigned *seen_address;
static app_pc return_marker, unblock_marker;
static unsigned ordinary, admissions, commits, entries, markers, senders;
static bool terminal;
static const uint64_t child_bit = UINT64_C(1) << (SIGCHLD - 1);

static void require(bool condition, const char *message) {
  if (!condition) {
    dr_fprintf(STDERR, "capture client failure case=%s: %s\n", mode, message);
    dr_exit_process(43);
  }
}

static bool is_case(const char *name) { return strcmp(mode, name) == 0; }
static bool is_parent(void) { return dr_get_process_id() == original_pid; }
static bool guest_blocked(void) {
  return is_case("guest-blocked") || is_case("physical-blocked");
}
static unsigned expected_events(void) {
  if (is_case("ignore-refused") || is_case("autoreap-refused") ||
      is_case("no-notification") || terminal) return 0;
  return is_case("stale-domain") ? 2 : 1;
}

static int read_int(const int *address) {
  int value;
  require(address != NULL && dr_safe_read(address, sizeof(value), &value, NULL),
          "read permanent fixture integer");
  return value;
}

static uint64_t guest_mask(void) {
  uint64_t value;
  require(mask_address != NULL && dr_safe_read(mask_address, sizeof(value), &value, NULL),
          "read complete fixture guest mask");
  return value;
}

/* Direct native access observes the physical mask, not DR's virtual app view.
 * Writes only establish/undo test setup outside the public capture lease. */
static uint64_t physical_mask(const uint64_t *set) {
  register long size __asm__("r10") = 8;
  uint64_t old = 0;
  long result;
  __asm__ volatile("syscall" : "=a"(result)
                   : "0"((long)SYS_rt_sigprocmask), "D"((long)SIG_SETMASK),
                     "S"(set), "d"(&old), "r"(size)
                   : "rcx", "r11", "cc", "memory");
  require(result == 0, "native physical-mask operation");
  return old;
}

static uint64_t physical_pending(void) {
  uint64_t pending = 0;
  long result;
  __asm__ volatile("syscall" : "=a"(result)
                   : "0"((long)SYS_rt_sigpending), "D"(&pending), "S"(8L)
                   : "rcx", "r11", "cc", "memory");
  require(result == 0, "query physical pending set");
  return pending;
}

static reg_t invoke(void *dc, int number, int count, const reg_t *args) {
  dr_syscall_invoke_result_t result;
  require(dr_invoke_syscall_as_app_with_result(dc, number, count, args, &result) &&
              result.interruption == 0, "ordinary injection inside live owner callback");
  return result.value;
}

static void publish_child(void *dc, unsigned index) {
  char byte = 'x';
  int fd = read_int(release_address + index);
  reg_t write_args[] = { (reg_t)fd, (reg_t)&byte, 1 };
  require(invoke(dc, SYS_write, 3, write_args) == 1, "independent child release");
  reg_t close_args[] = { (reg_t)fd };
  require(invoke(dc, SYS_close, 1, close_args) == 0, "release pipe writer closed");
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  int options = WEXITED | WNOWAIT;
  if (is_case("no-notification")) options |= __WCLONE;
  reg_t wait_args[] = { P_PID, (reg_t)read_int(children_address + index),
                        (reg_t)&info, (reg_t)options, 0 };
  require(invoke(dc, SYS_waitid, 5, wait_args) == 0 &&
              info.si_pid == read_int(children_address + index) &&
              info.si_signo == SIGCHLD && info.si_code == CLD_EXITED &&
              info.si_status == 31 + (int)index,
          "real-parent WNOWAIT observes exact independent child exit without reaping");
}

static void require_live(void *dc) {
  state_t *s = drmgr_get_tls_field(dc, tls_index);
  require(s != NULL && s->inside_callback && s->admission_token == 0 &&
              ordinary == 0 && admissions == 0 && commits == 0,
          "captured domain cannot enter ordinary or owned callback under live owner");
}

static void check_receipt(void *dc, const dr_signal_capture_handle_t *handle,
                          const dr_signal_capture_receipt_t *receipt,
                          uint64_t physical, unsigned count) {
  siginfo_t info;
  _Static_assert(sizeof(info) == sizeof(receipt->siginfo), "exact Linux siginfo bytes");
  memcpy(&info, receipt->siginfo, sizeof(info));
  require(receipt->handle.owner_epoch == handle->owner_epoch &&
              receipt->handle.token == handle->token &&
              receipt->handle.owner_pid == original_pid &&
              receipt->handle.owner_tid == (int)dr_get_thread_id(dc) &&
              receipt->version != 0 && receipt->raw_entries == count &&
              receipt->linked == count && receipt->coalesced == 0 &&
              receipt->pending_records == count &&
              receipt->guest_mask == guest_mask() &&
              receipt->saved_physical_mask == physical && physical_mask(NULL) == physical,
          "same-owner raw-to-linked receipt and both complete masks");
  require(info.si_signo == SIGCHLD && info.si_code == CLD_EXITED &&
              info.si_pid == read_int(children_address + count - 1) &&
              info.si_status == 30 + (int)count,
          "receipt is the current authentic retained child siginfo");
  require_live(dc);
}

static void owner_callback(void *dc) {
  state_t *s = drmgr_get_tls_field(dc, tls_index);
  require(s != NULL && !s->inside_callback && entries++ == 0, "single capture callback");
  s->inside_callback = true;
  uint64_t original_physical = physical_mask(NULL);
  uint64_t physical = original_physical;
  if (is_case("physical-blocked")) {
    physical |= child_bit;
    physical_mask(&physical);
  }
  dr_signal_capture_handle_t handle;
  dr_signal_capture_receipt_t receipt;
  if (is_case("ignore-refused") || is_case("autoreap-refused")) {
    require(dr_signal_capture_begin(dc, 1, &handle) == DR_SIGNAL_CAPTURE_UNAVAILABLE &&
                physical_mask(NULL) == physical, "ineligible guest action refuses without mutation");
  } else {
    require(dr_signal_capture_begin(dc, 1, &handle) == DR_SIGNAL_CAPTURE_OK &&
                handle.owner_epoch == 1 && handle.token != 0 &&
                handle.owner_pid == original_pid &&
                handle.owner_tid == (int)dr_get_thread_id(dc) &&
                physical_mask(NULL) == (physical | child_bit), "Begin owns and excludes SIGCHLD");
    dr_signal_capture_handle_t wrong = handle;
    wrong.owner_tid = -1;
    require(dr_signal_capture_close(dc, &wrong, &receipt) == DR_SIGNAL_CAPTURE_STALE &&
                dr_signal_capture_cancel(dc, &wrong) == DR_SIGNAL_CAPTURE_STALE &&
                physical_mask(NULL) == (physical | child_bit), "wrong owner cannot close or cancel");
    wrong = handle;
    ++wrong.owner_epoch;
    require(dr_signal_capture_close(dc, &wrong, &receipt) == DR_SIGNAL_CAPTURE_STALE &&
                physical_mask(NULL) == (physical | child_bit), "wrong epoch cannot restore mask");
    if (is_case("premature")) {
      require(dr_signal_capture_close(dc, &handle, &receipt) == DR_SIGNAL_CAPTURE_NO_RECORD &&
                  receipt.raw_entries == 0 && receipt.linked == 0 && receipt.pending_records == 0 &&
                  physical_mask(NULL) == physical, "Close before child publication is not Captured");
      require(dr_signal_capture_release(dc, &receipt) == DR_SIGNAL_CAPTURE_STALE,
              "no-record snapshot cannot release a captured domain");
    }
    publish_child(dc, 0);
    if (is_case("coalescing")) {
      require(physical_mask(NULL) == (physical | child_bit) &&
                  (physical_pending() & child_bit) != 0,
              "first authentic publication is still physically pending");
      publish_child(dc, 1);
      require(physical_mask(NULL) == (physical | child_bit) &&
                  (physical_pending() & child_bit) != 0,
              "both publications share the unchanged standard-signal pending domain");
    }
    require_live(dc);
    dr_signal_capture_status_t status = dr_signal_capture_close(dc, &handle, &receipt);
    if (is_case("no-notification") || is_case("physical-blocked")) {
      dr_signal_capture_status_t expected = is_case("physical-blocked")
          ? DR_SIGNAL_CAPTURE_KERNEL_BLOCKED : DR_SIGNAL_CAPTURE_NO_RECORD;
      require(status == expected && receipt.raw_entries == 0 && receipt.linked == 0 &&
                  receipt.pending_records == 0 && receipt.guest_mask == guest_mask() &&
                  receipt.saved_physical_mask == physical && physical_mask(NULL) == physical,
              "finite literal no-record or kernel-blocked result is not capture");
      require(dr_signal_capture_cancel(dc, &handle) == DR_SIGNAL_CAPTURE_OK,
              "finite unobserved outcome cancels without a signal wait");
    } else {
      require(status == DR_SIGNAL_CAPTURE_OK, "Close observes actual retained capture");
      check_receipt(dc, &handle, &receipt, physical, 1);
      if (is_case("stale-domain")) {
        /* The second child is not released until the first authentic record is
         * already retained. This tests a real domain mutation, not an invented
         * generic promise of one notification for every child exit. */
        dr_signal_capture_receipt_t old = receipt;
        publish_child(dc, 1);
        require(dr_signal_capture_release(dc, &old) == DR_SIGNAL_CAPTURE_STALE &&
                    physical_mask(NULL) == physical, "real later capture makes old receipt stale");
        require(dr_signal_capture_close(dc, &handle, &receipt) == DR_SIGNAL_CAPTURE_OK &&
                    receipt.version != old.version, "refresh owns the changed domain");
        check_receipt(dc, &handle, &receipt, physical, 2);
      }
      dr_signal_capture_receipt_t wrong_receipt = receipt;
      wrong_receipt.handle.owner_pid = -1;
      require(dr_signal_capture_release(dc, &wrong_receipt) == DR_SIGNAL_CAPTURE_STALE &&
                  physical_mask(NULL) == physical, "wrong-owner release is nonmutating");
      dr_signal_capture_receipt_t unchanged;
      require(dr_signal_capture_close(dc, &handle, &unchanged) == DR_SIGNAL_CAPTURE_OK &&
                  memcmp(&receipt, &unchanged, sizeof(receipt)) == 0,
              "failed release preserves the exact owned receipt");
      if (is_case("terminal")) {
        /* Reap before terminating the owner; no orphan/zombie is part of this
         * test. Terminal teardown may retire its own undelivered domain. */
        siginfo_t info;
        reg_t args[] = { P_PID, (reg_t)read_int(children_address), (reg_t)&info, WEXITED, 0 };
        require(invoke(dc, SYS_waitid, 5, args) == 0 && info.si_pid == read_int(children_address),
                "terminal owner leaves no unreaped fixture child");
        terminal = true;
        dr_fprintf(STDOUT, "capture terminal retained=1 admissions=0 status=31\n");
        dr_exit_process(31);
        require(false, "terminal owner cannot continue application");
      }
      if (is_case("cancel"))
        require(dr_signal_capture_cancel(dc, &handle) == DR_SIGNAL_CAPTURE_OK,
                "live Cancel preserves the authentic pending domain");
      else
        require(dr_signal_capture_release(dc, &receipt) == DR_SIGNAL_CAPTURE_OK,
                "release current receipt to normal unwind");
    }
    require(dr_signal_capture_release(dc, &receipt) == DR_SIGNAL_CAPTURE_STALE &&
                dr_signal_capture_cancel(dc, &handle) == DR_SIGNAL_CAPTURE_STALE &&
                physical_mask(NULL) == physical, "consumed lease refuses duplicates without mutation");
    if (is_case("physical-blocked")) {
      require((physical_pending() & child_bit) != 0 && (original_physical & child_bit) == 0,
              "cancellation preserved the authentic kernel-pending notification");
      /* Cancel already restored the exact saved mask, including our private
       * setup bit. Virtual sigprocmask does not undo that native setup. Remove
       * it explicitly only after checking cancellation, while WAIT and the
       * internal release-pending state still forbid callback reentry. The guest
       * remains virtually blocked until its separate legal unblock syscall. */
      physical_mask(&original_physical);
      require(physical_mask(NULL) == original_physical, "undo native test mask setup");
    }
  }
  require_live(dc);
  s->inside_callback = false;
  dr_syscall_set_result(dc, (reg_t)read_int(parent_address));
}

static dr_signal_action_t ordinary_signal(void *dc, dr_siginfo_t *event) {
  state_t *s = drmgr_get_tls_field(dc, tls_index);
  require(is_parent() && event->sig == SIGCHLD && s != NULL && !s->inside_callback,
          "ordinary callback occurs only after live native callback unwinds");
  ++ordinary;
  return DR_SIGNAL_DELIVER;
}

static dr_signal_admission_status_t admission(
    void *dc, const dr_signal_admission_t *event, dr_mcontext_t *redirect) {
  state_t *s = drmgr_get_tls_field(dc, tls_index);
  if (event == NULL) {
    if (!is_parent() || s == NULL || parent_address == NULL) return DR_SIGNAL_ADMISSION_BYPASS;
    return s->inside_callback || s->admission_token ? DR_SIGNAL_ADMISSION_WAIT
                                                   : DR_SIGNAL_ADMISSION_START;
  }
  require(is_parent() && s != NULL && !s->inside_callback && s->admission_token == 0 &&
              event->sig == SIGCHLD && event->token != 0 && redirect != NULL &&
              ordinary == admissions + 1, "one ordinary event precedes each authentic admission");
  siginfo_t info;
  memcpy(&info, event->siginfo, sizeof(info));
  unsigned index = info.si_pid == read_int(children_address) ? 0 : 1;
  require(index < expected_events() && info.si_pid == read_int(children_address + index) &&
              info.si_signo == SIGCHLD && info.si_code == CLD_EXITED &&
              info.si_status == 31 + (int)index && !(senders & (1u << index)),
          "authentic controlled child provenance survives cancellation or release");
  senders |= 1u << index;
  s->admission_token = event->token;
  s->phase = 1;
  ++admissions;
  redirect->pc = stub;
  redirect->xax = SYS_gettid;
  return DR_SIGNAL_ADMISSION_START;
}

static bool filter_syscall(void *dc, int number) { (void)dc; (void)number; return true; }

static bool pre_syscall(void *dc, int number) {
  state_t *s = drmgr_get_tls_field(dc, tls_index);
  if (s != NULL && s->phase == 1) {
    require(number == SYS_gettid && s->admission_token != 0, "private admission continuation");
    s->phase = 2;
    dr_syscall_set_result(dc, 0);
    return false;
  }
  if (is_parent() && number == SYS_gettid && stage_address != NULL &&
      read_int(stage_address) == 1 && entries == 0) {
    owner_callback(dc);
    return false;
  }
  return true;
}

static void finish_admission(void) {
  void *dc = dr_get_current_drcontext();
  state_t *s = drmgr_get_tls_field(dc, tls_index);
  require(s != NULL && !s->inside_callback && s->phase == 2 && s->admission_token != 0,
          "private continuation completes after callback unwind");
  uint64 token = s->admission_token;
  s->admission_token = 0;
  s->phase = 0;
  ++commits;
  atomic_store_explicit(seen_address, commits, memory_order_release);
  dr_signal_admission_resume(dc, token, SIGCHLD, NULL, NULL);
  require(false, "valid admission resume cannot return");
}

static void after_syscall(bool unblock) {
  unsigned expected = !unblock && guest_blocked() ? 0 : expected_events();
  require(entries == 1 && ordinary == expected && admissions == expected && commits == expected,
          "exact delivery before the first resumed application instruction");
  ++markers;
}

static dr_emit_flags_t instrument(void *dc, void *tag, instrlist_t *bb,
                                 instr_t *instruction, bool for_trace,
                                 bool translating, void *data) {
  (void)tag; (void)for_trace; (void)translating; (void)data;
  if (instr_is_app(instruction)) {
    app_pc pc = instr_get_app_pc(instruction);
    if (pc == stub + 2)
      dr_insert_clean_call(dc, bb, instruction, (void *)finish_admission, false, 0);
    else if (pc == return_marker || pc == unblock_marker)
      dr_insert_clean_call(dc, bb, instruction, (void *)after_syscall, false, 1,
                           OPND_CREATE_INT32(pc == unblock_marker));
  }
  return DR_EMIT_DEFAULT;
}

static void module_load(void *dc, const module_data_t *module, bool loaded) {
  (void)dc; (void)loaded;
  const int *found = (const int *)dr_get_proc_address(module->handle, "capture_parent");
  if (found == NULL) return;
  require(parent_address == NULL, "one capture guest image");
  parent_address = found;
  children_address = (const int *)dr_get_proc_address(module->handle, "capture_children");
  release_address = (const int *)dr_get_proc_address(module->handle, "capture_release_fds");
  stage_address = (const int *)dr_get_proc_address(module->handle, "capture_stage");
  mask_address = (const uint64_t *)dr_get_proc_address(module->handle, "capture_guest_mask");
  seen_address = (_Atomic unsigned *)dr_get_proc_address(module->handle, "capture_seen");
  return_marker = (app_pc)dr_get_proc_address(module->handle, "capture_return_marker");
  unblock_marker = (app_pc)dr_get_proc_address(module->handle, "capture_unblock_marker");
  require(children_address && release_address && stage_address && mask_address &&
              seen_address && return_marker && unblock_marker, "all permanent fixture symbols");
}

static void thread_init(void *dc) {
  state_t *s = dr_thread_alloc(dc, sizeof(*s));
  require(s != NULL, "capture TLS allocation");
  memset(s, 0, sizeof(*s));
  require(drmgr_set_tls_field(dc, tls_index, s), "capture TLS publication");
}

static void thread_exit(void *dc) {
  state_t *s = drmgr_get_tls_field(dc, tls_index);
  require(s != NULL && s->admission_token == 0 && s->phase == 0 &&
              (!s->inside_callback || terminal), "no live admission at thread termination");
  dr_thread_free(dc, s, sizeof(*s));
}

static void exit_event(void) {
  if (is_parent()) {
    unsigned expected = expected_events();
    require(entries == 1 && ordinary == expected && admissions == expected && commits == expected &&
                markers == (terminal ? 0u : guest_blocked() ? 2u : 1u) &&
                senders == ((1u << expected) - 1), "exact terminal capture accounting");
    dr_fprintf(STDOUT, "capture client completed case=%s ordinary=%u admissions=%u commits=%u\n",
               mode, ordinary, admissions, commits);
  }
  drmgr_unregister_bb_insertion_event(instrument);
  drmgr_unregister_pre_syscall_event(pre_syscall);
  drmgr_unregister_filter_syscall_event(filter_syscall);
  drmgr_unregister_signal_event(ordinary_signal);
  require(dr_unregister_signal_admission_event(admission), "unregister owned admission");
  drmgr_unregister_module_load_event(module_load);
  drmgr_unregister_thread_init_event(thread_init);
  drmgr_unregister_thread_exit_event(thread_exit);
  drmgr_unregister_tls_field(tls_index);
  dr_nonheap_free(stub, dr_page_size());
  drmgr_exit();
}

DR_EXPORT void dr_client_main(client_id_t id, int argc, const char *argv[]) {
  (void)id;
  if (argc == 2 && strlen(argv[1]) < sizeof(mode))
    memcpy(mode, argv[1], strlen(argv[1]) + 1);
  require(argc == 2 && (is_case("release") || is_case("caught") || is_case("cancel") ||
              is_case("premature") || is_case("stale-domain") || is_case("coalescing") ||
              is_case("guest-blocked") ||
              is_case("physical-blocked") || is_case("no-notification") ||
              is_case("ignore-refused") || is_case("autoreap-refused") || is_case("terminal")),
          "one exact capture mode");
  original_pid = dr_get_process_id();
  uint64 code_api = 0, intercept_all = 0;
  require(dr_get_integer_option("code_api", &code_api) && code_api == 1 &&
              dr_get_integer_option("intercept_all_signals", &intercept_all) && intercept_all == 1,
          "actual fixture uses full client signal interception");
  require(drmgr_init(), "capture drmgr");
  tls_index = drmgr_register_tls_field();
  require(tls_index >= 0, "capture TLS index");
  stub = dr_nonheap_alloc(dr_page_size(), DR_MEMPROT_READ | DR_MEMPROT_WRITE | DR_MEMPROT_EXEC);
  require(stub != NULL, "private admission stub");
  memcpy(stub, "\x0f\x05\x0f\x0b", 4);
  require(drmgr_register_thread_init_event(thread_init) &&
              drmgr_register_thread_exit_event(thread_exit) &&
              drmgr_register_module_load_event(module_load) &&
              drmgr_register_bb_instrumentation_event(NULL, instrument, NULL) &&
              drmgr_register_filter_syscall_event(filter_syscall) &&
              drmgr_register_pre_syscall_event(pre_syscall) &&
              drmgr_register_signal_event(ordinary_signal) &&
              drmgr_register_exit_event(exit_event) &&
              dr_register_signal_admission_event(admission), "register capture events");
}
