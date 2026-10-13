/* Copyright (c) Meta Platforms, Inc. and affiliates. BSD-style license: LICENSE. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include "dr_api.h"
#include "drmgr.h"

/* This exercises real SDK callbacks and kernel publication. It does not claim
 * that this C readiness callback is the Hermit Rust-future retirement test. */
static int parent_pid, tls_index;
static char mode[32];
static const int *parent_address, *child_address, *release_address, *stage_address;
static app_pc return_marker;
static byte *stub;
static unsigned wait_entries, stage_refusals, ordinary, admissions, commits, completions;
static bool published, staged, final_checkpoint;
static uint64 captured_allocation;
typedef struct {
  int readiness; /* 0 BYPASS, 1 WAIT, 2 START: actual registered admission return. */
  uint64 token;
  unsigned admission_phase;
} thread_state;

static bool is_case(const char *value) { return strcmp(mode, value) == 0; }
static bool is_parent(void) { return dr_get_process_id() == parent_pid; }
static bool terminal_case(void) {
  return is_case("late-arg") || is_case("late-skip") || is_case("late-readiness");
}
static void require(bool condition, const char *message) {
  if (!condition) {
    dr_fprintf(STDERR, "wait finalizer client failure case=%s: %s\n", mode, message);
    dr_exit_process(43);
  }
}
static int read_int(const int *address) {
  int value;
  require(address != NULL && dr_safe_read(address, sizeof(value), &value, NULL),
          "read permanent fixture integer");
  return value;
}
static uint64 physical_mask(void) {
  register long size __asm__("r10") = 8;
  uint64 value = 0;
  long result;
  __asm__ volatile("syscall" : "=a"(result)
                   : "0"((long)SYS_rt_sigprocmask), "D"((long)SIG_SETMASK),
                     "S"((const void *)NULL), "d"(&value), "r"(size)
                   : "rcx", "r11", "cc", "memory");
  require(result == 0, "read literal physical mask");
  return value;
}
static reg_t invoke(void *dc, int nr, int argc, const reg_t *args) {
  dr_syscall_invoke_result_t result;
  require(dr_invoke_syscall_as_app_with_result(dc, nr, argc, args, &result) &&
              result.interruption == 0, "real returning application injection");
  return result.value;
}
static siginfo_t publish(void *dc, bool reap) {
  require(!published, "child released once");
  published = true;
  char value = 'x';
  reg_t write_args[] = {(reg_t)read_int(release_address), (reg_t)&value, 1};
  require(invoke(dc, SYS_write, 3, write_args) == 1, "release independent child");
  reg_t close_args[] = {(reg_t)read_int(release_address)};
  require(invoke(dc, SYS_close, 1, close_args) == 0, "close child release writer");
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  reg_t wait_args[] = {P_PID, (reg_t)read_int(child_address), (reg_t)&info,
                       WEXITED | WNOWAIT, 0};
  require(invoke(dc, SYS_waitid, 5, wait_args) == 0 &&
              info.si_signo == SIGCHLD && info.si_pid == read_int(child_address) &&
              info.si_code == CLD_EXITED && info.si_status == 31,
          "positive exact-child parent WNOWAIT proof");
  if (reap) {
    wait_args[3] = WEXITED;
    require(invoke(dc, SYS_waitid, 5, wait_args) == 0 &&
                info.si_pid == read_int(child_address) && info.si_status == 31,
            "terminal negative leaves no fixture zombie");
  }
  return info;
}
static dr_signal_publication_owner_t owner(uint64 epoch) {
  dr_signal_publication_owner_t value = {0};
  value.image_epoch = 1;
  value.callback_epoch = epoch;
  value.callback_kind = DR_SIGNAL_PUBLICATION_SYSCALL;
  return value;
}
static dr_signal_domain_snapshot_t observe(void *dc, const dr_signal_publication_owner_t *o) {
  dr_signal_domain_snapshot_t value;
  require(dr_signal_domain_observe(dc, o, &value) == DR_SIGNAL_CAPTURE_OK &&
              value.domain != 0 && value.owner_pid == parent_pid &&
              value.owner_tid == (int)dr_get_thread_id(dc) &&
              memcmp(&value.owner, o, sizeof(*o)) == 0,
          "real complete owner/domain snapshot");
  return value;
}
static void refused(void *dc, const dr_signal_publication_owner_t *auth,
                    const dr_signal_publication_owner_t *supplied,
                    int nr, app_pc pc, const reg_t args[6], const char *label) {
  dr_signal_domain_snapshot_t before = observe(dc, auth);
  uint64 mask = physical_mask();
  require(dr_signal_publication_stage_wait_return(dc, supplied, nr, pc, args) ==
              DR_SIGNAL_CAPTURE_STALE, label);
  dr_signal_domain_snapshot_t after = observe(dc, auth);
  require(memcmp(&before, &after, sizeof(before)) == 0 && physical_mask() == mask,
          "rejected staging changes neither domain nor literal mask");
  ++stage_refusals;
}

static void first_wait(void *dc, int nr) {
  thread_state *s = drmgr_get_tls_field(dc, tls_index);
  require(s != NULL && s->token == 0 && !staged, "first actual wait callback");
  require(read_int(parent_address) == parent_pid, "actual parent identity");
  s->readiness = 1;
  dr_mcontext_t mc = {sizeof(mc), DR_MC_CONTROL};
  require(dr_get_mcontext(dc, &mc) && mc.pc == return_marker, "actual original wait PC");
  reg_t args[6];
  for (int i = 0; i != 6; ++i) args[i] = dr_syscall_get_param(dc, i);
  dr_signal_publication_owner_t auth = owner(1);
  dr_signal_domain_snapshot_t initial = observe(dc, &auth);
  require(initial.phase == 0 && initial.record_count == 0, "initial real empty domain");
  uint64 physical = physical_mask();
  dr_signal_publication_request_t request = {0};
  request.owner = auth; request.domain = initial.domain;
  request.reservation = 1; request.child_generation = 1;
  request.child_pid = read_int(child_address);
  dr_signal_publication_handle_t handle;
  require(dr_signal_publication_arm(dc, &request, &handle) == DR_SIGNAL_CAPTURE_OK &&
              physical_mask() == (physical | (UINT64_C(1) << (SIGCHLD - 1))),
          "real Arm excludes physical publication");
  refused(dc, &auth, &auth, nr, mc.pc, args, "live phase1 cannot stage");
  if (is_case("cancel-empty")) {
    require(dr_signal_publication_cancel(dc, &handle) == DR_SIGNAL_CAPTURE_OK,
            "Cancel of actual empty interval");
    dr_signal_domain_snapshot_t empty = observe(dc, &auth);
    require(empty.phase == 3 && empty.record_count == 0 && !published,
            "no-record Cancel is not a capture or delivery");
  } else {
    siginfo_t info = publish(dc, terminal_case());
    dr_signal_publication_receipt_t receipt;
    require(dr_signal_publication_close(dc, &handle, (const byte *)&info, &receipt) ==
                DR_SIGNAL_CAPTURE_OK && receipt.raw_entries == 1 &&
                receipt.outcome == DR_SIGNAL_PUBLICATION_LINKED && receipt.victim == 0 &&
                receipt.allocation != 0 && receipt.guest_mask == initial.guest_mask &&
                receipt.saved_physical_mask == physical,
            "real Close owns exact child, raw bytes, linked allocation and masks");
    captured_allocation = receipt.allocation;
    siginfo_t incoming; memcpy(&incoming, receipt.incoming_siginfo, sizeof(incoming));
    require(incoming.si_signo == SIGCHLD && incoming.si_pid == info.si_pid &&
                incoming.si_code == info.si_code && incoming.si_status == info.si_status,
            "kernel notification matches parent WNOWAIT terminal identity");
    dr_signal_domain_snapshot_t held = observe(dc, &auth);
    require(held.phase == 2 && held.record_count == 1 && held.queued == 1 &&
                held.active == 0 && held.returning == 0 &&
                held.records[0].generation == captured_allocation &&
                memcmp(held.records[0].siginfo, receipt.incoming_siginfo,
                       sizeof(receipt.incoming_siginfo)) == 0,
            "full raw notification bytes match actual retained allocation");
    refused(dc, &auth, &auth, nr, mc.pc, args, "live phase2 cannot stage");
    require((is_case("waitid-cancel")
                 ? dr_signal_publication_cancel(dc, &handle)
                 : dr_signal_publication_release(dc, &receipt)) == DR_SIGNAL_CAPTURE_OK,
            "exact real native Release or Cancel");
  }
  require(physical_mask() == physical, "terminal operation restored literal mask");
  refused(dc, &auth, &auth, nr, mc.pc, args, "real admission WAIT cannot stage");
  s->readiness = 0;
  refused(dc, &auth, &auth, nr, mc.pc, args, "BYPASS is not START permission");
  s->readiness = 2;
  dr_signal_publication_owner_t wrong = auth;
  ++wrong.callback_epoch;
  refused(dc, &auth, &wrong, nr, mc.pc, args, "wrong callback epoch refused");
  wrong = auth; ++wrong.image_epoch;
  refused(dc, &auth, &wrong, nr, mc.pc, args, "wrong image refused");
  wrong = auth; wrong.admission_token = 1;
  refused(dc, &auth, &wrong, nr, mc.pc, args, "foreign admission token refused");
  refused(dc, &auth, &auth, SYS_getpid, mc.pc, args, "wrong syscall refused");
  refused(dc, &auth, &auth, nr, mc.pc + 1, args, "wrong original PC refused");
  for (int i = 0; i != 6; ++i) {
    reg_t wrong_args[6]; memcpy(wrong_args, args, sizeof(args));
    wrong_args[i] ^= 1;
    refused(dc, &auth, &auth, nr, mc.pc, wrong_args, "every supplied operand is exact");
  }
  dr_syscall_set_param(dc, 5, args[5] ^ 1);
  refused(dc, &auth, &auth, nr, mc.pc, args, "effective context also must match Arm");
  dr_syscall_set_param(dc, 5, args[5]);
  require(dr_signal_publication_stage_wait_return(dc, &auth, nr, mc.pc, args) ==
              DR_SIGNAL_CAPTURE_OK, "stage authentic original wait after START");
  staged = true;
  refused(dc, &auth, &auth, nr, mc.pc, args, "one-shot stage rejects duplicate");
  /* This is a real nested returning injection after permission was staged.
   * Its skipped-client pre path must not consume the outer wait permission. */
  dr_signal_domain_snapshot_t before_nested = observe(dc, &auth);
  require(invoke(dc, SYS_getpid, 0, NULL) == (reg_t)parent_pid,
          "nested returning syscall completes without consuming outer permission");
  dr_signal_domain_snapshot_t after_nested = observe(dc, &auth);
  require(memcmp(&before_nested, &after_nested, sizeof(before_nested)) == 0 &&
              physical_mask() == physical,
          "nested injection preserves original phase3 domain and literal mask");
  dr_fprintf(STDOUT, "wait finalizer staged case=%s refusals=%u captured=%d\n",
             mode, stage_refusals, captured_allocation != 0);
}

static dr_signal_admission_status_t admission(void *dc, const dr_signal_admission_t *event,
                                              dr_mcontext_t *redirect) {
  thread_state *s = drmgr_get_tls_field(dc, tls_index);
  if (event == NULL) {
    if (!is_parent() || s == NULL || parent_address == NULL) return DR_SIGNAL_ADMISSION_BYPASS;
    if (s->token != 0 || s->readiness == 1) return DR_SIGNAL_ADMISSION_WAIT;
    return s->readiness == 2 ? DR_SIGNAL_ADMISSION_START : DR_SIGNAL_ADMISSION_BYPASS;
  }
  require(is_parent() && s != NULL && s->readiness == 2 && s->token == 0 &&
              event->sig == SIGCHLD && event->token != 0 && ordinary == 1 &&
              admissions++ == 0, "actual owned admission follows real unwind");
  siginfo_t info; memcpy(&info, event->siginfo, sizeof(info));
  require(info.si_pid == read_int(child_address) && info.si_code == CLD_EXITED &&
              info.si_status == 31 &&
              (captured_allocation == 0 ||
               event->publication_allocation == captured_allocation),
          "actual child siginfo/allocation survives finalizer");
  s->token = event->token; s->admission_phase = 1;
  redirect->pc = stub; redirect->xax = SYS_gettid;
  return DR_SIGNAL_ADMISSION_START;
}
static dr_signal_action_t ordinary_signal(void *dc, dr_siginfo_t *event) {
  thread_state *s = drmgr_get_tls_field(dc, tls_index);
  require(is_parent() && s != NULL && s->readiness == 2 && event->sig == SIGCHLD &&
              ordinary++ == 0, "ordinary signal cannot run under the test borrow");
  return DR_SIGNAL_DELIVER;
}
static bool filter_syscall(void *dc, int nr) { (void)dc; (void)nr; return true; }
static bool pre_syscall(void *dc, int nr) {
  thread_state *s = drmgr_get_tls_field(dc, tls_index);
  if (s != NULL && s->admission_phase == 1) {
    require(nr == SYS_gettid && s->token != 0, "actual private admission syscall");
    s->admission_phase = 2; dr_syscall_set_result(dc, 0); return false;
  }
  if (!is_parent() || stage_address == NULL) return true;
  if (read_int(stage_address) == 1 && (nr == SYS_wait4 || nr == SYS_waitid)) {
    ++wait_entries;
    if (wait_entries == 1) first_wait(dc, nr);
    else if (is_case("cancel-empty") && !published) {
      dr_signal_publication_owner_t current = owner(wait_entries);
      dr_signal_domain_snapshot_t empty = observe(dc, &current);
      require(empty.phase == 0 && empty.record_count == 0 && admissions == 0,
              "no-record cancellation resumes normally without invented delivery");
      s->readiness = 1; (void)publish(dc, false); s->readiness = 2;
    }
  }
  if (read_int(stage_address) == 2 && nr == SYS_getuid &&
      dr_syscall_get_param(dc, 0) == 0x5746494e) {
    dr_signal_publication_owner_t current = owner(wait_entries + 1);
    dr_signal_domain_snapshot_t done = observe(dc, &current);
    require(staged && completions == 1 && ordinary == 1 && admissions == 1 && commits == 1 &&
                done.phase == 0 && done.record_count == 0 && done.retirements == 1 &&
                (captured_allocation == 0 || done.last_retired == captured_allocation),
            "actual syscall continuation and allocation retired exactly once");
    final_checkpoint = true; dr_syscall_set_result(dc, 77); return false;
  }
  return true;
}
static bool late_pre_syscall(void *dc, int nr) {
  if (!is_parent() || !staged || wait_entries != 1 ||
      (nr != SYS_wait4 && nr != SYS_waitid)) return true;
  if (is_case("late-arg")) {
    dr_syscall_set_param(dc, 5, dr_syscall_get_param(dc, 5) ^ 1);
    dr_fprintf(STDOUT, "wait finalizer late mutation arg5; required terminal before kernel\n");
  } else if (is_case("late-skip")) {
    dr_fprintf(STDOUT, "wait finalizer late mutation execute=false; required terminal before kernel\n");
    return false;
  } else if (is_case("late-readiness")) {
    thread_state *s = drmgr_get_tls_field(dc, tls_index);
    s->readiness = 1;
    dr_fprintf(STDOUT, "wait finalizer late mutation readiness=WAIT; required terminal before kernel\n");
  }
  return true;
}
static void finish_admission(void) {
  void *dc = dr_get_current_drcontext();
  thread_state *s = drmgr_get_tls_field(dc, tls_index);
  require(s != NULL && s->admission_phase == 2 && s->token != 0,
          "real owned continuation returns once");
  uint64 token = s->token; s->token = 0; s->admission_phase = 0; ++commits;
  dr_signal_admission_resume(dc, token, is_case("wait4-suppress") ? 0 : SIGCHLD, NULL, NULL);
  require(false, "valid resume does not return");
}
static void after_wait(void) {
  require(!terminal_case() && ++completions == 1 && admissions == 1 && commits == 1,
          "actual admission precedes one application wait fallthrough");
}
static dr_emit_flags_t instrument(void *dc, void *tag, instrlist_t *bb,
                                  instr_t *instruction, bool trace, bool translating, void *data) {
  (void)tag; (void)trace; (void)translating; (void)data;
  if (instr_is_app(instruction)) {
    app_pc pc = instr_get_app_pc(instruction);
    if (pc == stub + 2)
      dr_insert_clean_call(dc, bb, instruction, (void *)finish_admission, false, 0);
    else if (pc == return_marker)
      dr_insert_clean_call(dc, bb, instruction, (void *)after_wait, false, 0);
  }
  return DR_EMIT_DEFAULT;
}
static void module_load(void *dc, const module_data_t *module, bool loaded) {
  (void)dc; (void)loaded;
  const int *found = (const int *)dr_get_proc_address(module->handle, "finalizer_parent");
  if (found == NULL) return;
  require(parent_address == NULL, "one fixture image"); parent_address = found;
  child_address = (const int *)dr_get_proc_address(module->handle, "finalizer_child");
  release_address = (const int *)dr_get_proc_address(module->handle, "finalizer_release_fd");
  stage_address = (const int *)dr_get_proc_address(module->handle, "finalizer_stage");
  return_marker = (app_pc)dr_get_proc_address(module->handle, "finalizer_wait_return");
  require(child_address && release_address && stage_address && return_marker, "all fixture symbols");
}
static void thread_init(void *dc) {
  thread_state *s = dr_thread_alloc(dc, sizeof(*s));
  require(s != NULL, "thread state allocation"); memset(s, 0, sizeof(*s));
  require(drmgr_set_tls_field(dc, tls_index, s), "actual thread state");
}
static void thread_exit(void *dc) {
  thread_state *s = drmgr_get_tls_field(dc, tls_index); dr_thread_free(dc, s, sizeof(*s));
}
static void exit_event(void) {
  if (is_parent() && !terminal_case()) {
    require(final_checkpoint, "final real guest checkpoint required");
    dr_fprintf(STDOUT, "wait finalizer client completed case=%s admissions=1 commits=1 continuations=1\n", mode);
  }
  drmgr_unregister_bb_insertion_event(instrument);
  drmgr_unregister_pre_syscall_event(pre_syscall);
  drmgr_unregister_pre_syscall_event(late_pre_syscall);
  drmgr_unregister_filter_syscall_event(filter_syscall);
  drmgr_unregister_signal_event(ordinary_signal);
  dr_unregister_signal_admission_event(admission);
  drmgr_unregister_module_load_event(module_load);
  drmgr_unregister_thread_init_event(thread_init);
  drmgr_unregister_thread_exit_event(thread_exit);
  drmgr_unregister_tls_field(tls_index);
  dr_nonheap_free(stub, dr_page_size()); drmgr_exit();
}
DR_EXPORT void dr_client_main(client_id_t id, int argc, const char *argv[]) {
  (void)id;
  require(argc == 2 && strlen(argv[1]) < sizeof(mode), "one finite mode");
  memcpy(mode, argv[1], strlen(argv[1]) + 1);
  require(is_case("wait4-release") || is_case("waitid-cancel") || is_case("wait4-suppress") ||
              is_case("cancel-empty") || terminal_case(), "known mode");
  parent_pid = dr_get_process_id();
  require(drmgr_init(), "drmgr init"); tls_index = drmgr_register_tls_field();
  require(tls_index >= 0, "TLS index");
  stub = dr_nonheap_alloc(dr_page_size(), DR_MEMPROT_READ | DR_MEMPROT_WRITE | DR_MEMPROT_EXEC);
  require(stub != NULL, "private real admission stub"); memcpy(stub, "\x0f\x05\x0f\x0b", 4);
  drmgr_priority_t first = {sizeof(first), "wait-finalizer-owner", NULL, NULL, -10};
  drmgr_priority_t last = {sizeof(last), "wait-finalizer-late", NULL, "wait-finalizer-owner", 10};
  require(drmgr_register_thread_init_event(thread_init) &&
              drmgr_register_thread_exit_event(thread_exit) &&
              drmgr_register_module_load_event(module_load) &&
              drmgr_register_bb_instrumentation_event(NULL, instrument, NULL) &&
              drmgr_register_filter_syscall_event(filter_syscall) &&
              drmgr_register_pre_syscall_event_ex(pre_syscall, &first) &&
              drmgr_register_pre_syscall_event_ex(late_pre_syscall, &last) &&
              drmgr_register_signal_event(ordinary_signal) &&
              dr_register_signal_admission_event(admission) &&
              drmgr_register_exit_event(exit_event), "actual callback registrations");
}
