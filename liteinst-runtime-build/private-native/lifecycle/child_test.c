#define _GNU_SOURCE
#include "child.h"
#include "crt_context.h"
#include "hermit-private-context-internal.h"
#include <asm/prctl.h>
#include <asm/unistd.h>
#include <assert.h>
#include <errno.h>
#include <futex-internal.h>
#include <limits.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/random.h>
#include <sys/resource.h>
#include <sys/rseq.h>

extern long reverie_preload_trusted_syscall(long, long, long, long, long, long, long);
extern long __real_pl_child_kernel(struct pl_child *);
extern void __real_pl_child_arrived(struct pl_child *) __attribute__((noreturn));
extern enum hermit_private_status __real___hermit_private_native_attach_v1
  (const struct hermit_private_native_v1 *);
extern enum hermit_private_status __real___hermit_private_native_retire_v1
  (const struct hermit_private_native_v1 *);
extern int __cxa_thread_atexit_impl(void (*)(void *), void *, void *);
extern void *__dso_handle;
extern int pl_child_test_capture_ok(const struct pl_child *);

static const struct pm_crt_context *control_root;
static const char *control_case;
static struct pl_child *control_child;
static pthread_key_t control_key;
static pthread_mutex_t control_mutex = PTHREAD_MUTEX_INITIALIZER;
static _Atomic unsigned tls_destructors, key_destructors, ran_body, rng_allocated;
static _Atomic unsigned signal_usr1, signal_usr2, signal_bad_tls;
static _Atomic uint32_t signal_attach_ready, signal_attach_release;
static _Atomic uint32_t signal_retire_ready, signal_retire_release;
static uint64_t signal_parent_fs, signal_parent_gs;
static uint64_t signal_original_mask, signal_unblocked_mask;
static long signal_parent_tid;
static __thread uint64_t thread_value = UINT64_C(0xa63b716ec5429d08);
static struct robust_list_head guest_robust;
static struct rseq guest_rseq __attribute__((aligned(32)));

static long control_raw(long number, long first, long second, long third, long fourth) {
  return reverie_preload_trusted_syscall(number, first, second, third, fourth, 0, 0);
}

static int selected(const char *name) {
  return strcmp(control_case, name) == 0;
}

static uint64_t signal_bit(int number) {
  return UINT64_C(1) << (number - 1);
}

static void signal_handler(int number) {
  uint64_t fs = 0, gs = 0;
  long tid = control_raw(__NR_gettid, 0, 0, 0, 0);
  if (control_raw(__NR_arch_prctl, ARCH_GET_FS, (long)&fs, 0, 0) != 0 ||
      control_raw(__NR_arch_prctl, ARCH_GET_GS, (long)&gs, 0, 0) != 0) {
    atomic_fetch_add_explicit(&signal_bad_tls, 1, memory_order_relaxed);
  } else if (control_child != NULL &&
             tid == (long)atomic_load_explicit(&control_child->kernel_tid,
                                               memory_order_relaxed)) {
    if (fs != control_child->private_fs || gs != control_child->inherited_gs)
      atomic_fetch_add_explicit(&signal_bad_tls, 1, memory_order_relaxed);
  } else if (tid != signal_parent_tid || fs != signal_parent_fs || gs != signal_parent_gs) {
    atomic_fetch_add_explicit(&signal_bad_tls, 1, memory_order_relaxed);
  }
  if (number == SIGUSR1)
    atomic_fetch_add_explicit(&signal_usr1, 1, memory_order_release);
  else if (number == SIGUSR2)
    atomic_fetch_add_explicit(&signal_usr2, 1, memory_order_release);
  else
    atomic_fetch_add_explicit(&signal_bad_tls, 1, memory_order_relaxed);
}

static void install_signal_controls(void) {
  struct sigaction action = {.sa_handler = signal_handler};
  assert(sigemptyset(&action.sa_mask) == 0);
  assert(sigaction(SIGUSR1, &action, NULL) == 0);
  assert(sigaction(SIGUSR2, &action, NULL) == 0);
  uint64_t signals = signal_bit(SIGUSR1) | signal_bit(SIGUSR2);
  assert(control_raw(__NR_rt_sigprocmask, SIG_UNBLOCK, (long)&signals,
                     (long)&signal_original_mask, sizeof signals) == 0);
  signal_parent_tid = control_raw(__NR_gettid, 0, 0, 0, 0);
  assert(control_raw(__NR_arch_prctl, ARCH_GET_FS, (long)&signal_parent_fs, 0, 0) == 0);
  assert(control_raw(__NR_arch_prctl, ARCH_GET_GS, (long)&signal_parent_gs, 0, 0) == 0);
}

static void assert_test_signals_blocked(void) {
  uint64_t mask = 0;
  assert(control_raw(__NR_rt_sigprocmask, SIG_SETMASK, 0, (long)&mask, sizeof mask) == 0);
  uint64_t catchable = UINT64_MAX & ~signal_bit(SIGKILL) & ~signal_bit(SIGSTOP);
  assert(mask == catchable);
  assert((mask & signal_bit(SIGUSR1)) != 0);
  assert((mask & signal_bit(SIGUSR2)) != 0);
  assert((mask & signal_bit(SIGKILL)) == 0);
  assert((mask & signal_bit(SIGSTOP)) == 0);
}

static void assert_test_signals_pending(void) {
  uint64_t pending = 0;
  assert(control_raw(__NR_rt_sigpending, (long)&pending, sizeof pending, 0, 0) == 0);
  assert((pending & signal_bit(SIGUSR1)) != 0);
  assert((pending & signal_bit(SIGUSR2)) != 0);
}

static void signal_boundary_arrive(_Atomic uint32_t *ready,
                                   _Atomic uint32_t *release) {
  assert_test_signals_blocked();
  atomic_store_explicit(ready, 1, memory_order_release);
  atomic_fetch_add_explicit(&control_child->event, 1, memory_order_release);
  (void)control_raw(__NR_futex, (long)&control_child->event,
                    FUTEX_WAKE_PRIVATE, INT_MAX, 0);
  for (;;) {
    uint32_t value = atomic_load_explicit(release, memory_order_acquire);
    if (value == 1) break;
    long result = control_raw(__NR_futex, (long)release,
                              FUTEX_WAIT_PRIVATE, value, 0);
    assert(result == 0 || result == -EAGAIN || result == -EINTR);
  }
  assert_test_signals_pending();
  assert(atomic_load_explicit(&signal_bad_tls, memory_order_acquire) == 0);
}

static void signal_boundary_release(_Atomic uint32_t *release) {
  atomic_store_explicit(release, 1, memory_order_release);
  assert(control_raw(__NR_futex, (long)release, FUTEX_WAKE_PRIVATE, 1, 0) >= 0);
}

static void wait_event(struct pl_child *child, uint32_t event) {
  long result = control_raw(__NR_futex, (long)&child->event,
                            FUTEX_WAIT | FUTEX_PRIVATE_FLAG, event, 0);
  assert(result == 0 || result == -EAGAIN || result == -EINTR);
}

static void wait_stage(struct pl_child *child, uint32_t expected) {
  for (;;) {
    uint32_t event = atomic_load_explicit(&child->event, memory_order_acquire);
    uint32_t stage = atomic_load_explicit(&child->stage, memory_order_acquire);
    if (stage == expected) return;
    assert(stage != PL_CHILD_POISONED);
    wait_event(child, event);
  }
}

long __wrap_pl_child_kernel(struct pl_child *child) {
  long result = __real_pl_child_kernel(child);
  if (selected("native-child-first") && result > 0) {
    wait_stage(child, PL_CHILD_ACTIVE);
    assert(atomic_load_explicit(&child->parent_arrived, memory_order_acquire) == 0);
    assert(atomic_load_explicit(&child->entry_release, memory_order_acquire) == 0);
    assert(atomic_load_explicit(&ran_body, memory_order_acquire) == 0);
  }
  return result;
}

void __wrap_pl_child_arrived(struct pl_child *child) {
  if (selected("native-parent-first")) {
    for (;;) {
      uint32_t event = atomic_load_explicit(&child->event, memory_order_acquire);
      if (atomic_load_explicit(&child->parent_arrived, memory_order_acquire) == 1) break;
      wait_event(child, event);
    }
    assert(atomic_load_explicit(&child->stage, memory_order_acquire) == PL_CHILD_STARTING);
  }
  __real_pl_child_arrived(child);
}

enum hermit_private_status __wrap___hermit_private_native_attach_v1
  (const struct hermit_private_native_v1 *native) {
  if (selected("native-signal-boundaries"))
    signal_boundary_arrive(&signal_attach_ready, &signal_attach_release);
  guest_robust.list = &guest_robust;
  assert(control_raw(__NR_set_robust_list, (long)&guest_robust, sizeof guest_robust, 0, 0) == 0);
  assert(control_raw(__NR_set_tid_address, (long)&control_child->kernel_tid, 0, 0, 0)
         == control_raw(__NR_gettid, 0, 0, 0, 0));
  assert(control_raw(__NR_rseq, (long)&guest_rseq, sizeof guest_rseq, 0, RSEQ_SIG) == 0);
  enum hermit_private_status status = __real___hermit_private_native_attach_v1(native);
  struct robust_list_head *observed = NULL;
  size_t length = 0;
  assert(control_raw(__NR_get_robust_list, 0, (long)&observed, (long)&length, 0) == 0);
  assert(observed == &guest_robust && length == sizeof guest_robust);
  return status;
}

enum hermit_private_status __wrap___hermit_private_native_retire_v1
  (const struct hermit_private_native_v1 *native) {
  if (selected("native-signal-boundaries"))
    signal_boundary_arrive(&signal_retire_ready, &signal_retire_release);
  enum hermit_private_status status = __real___hermit_private_native_retire_v1(native);
  if (status != HERMIT_PRIVATE_OK) return status;
  struct robust_list_head *observed = NULL;
  size_t length = 0;
  assert(control_raw(__NR_get_robust_list, 0, (long)&observed, (long)&length, 0) == 0);
  assert(observed == &guest_robust && length == sizeof guest_robust);
  assert(control_raw(__NR_rseq, (long)&guest_rseq, sizeof guest_rseq,
                     RSEQ_FLAG_UNREGISTER, RSEQ_SIG) == 0);
  assert(THREAD_SELF->getrandom_buf == NULL);
  if (selected("native-signal-boundaries")) {
    assert_test_signals_blocked();
    assert_test_signals_pending();
    assert(atomic_load_explicit(&signal_usr1, memory_order_acquire) == 1);
    assert(atomic_load_explicit(&signal_usr2, memory_order_acquire) == 1);
    assert(atomic_load_explicit(&signal_bad_tls, memory_order_acquire) == 0);
  }
  return status;
}

static void tls_destructor(void *argument) {
  assert(argument == control_child);
  assert(thread_value == UINT64_C(0x317dd83d72b6054e));
  atomic_fetch_add_explicit(&tls_destructors, 1, memory_order_release);
  if (selected("native-cleanup-reentry")) pl_child_retire(control_child);
}

static void key_destructor(void *argument) {
  assert(argument == control_child);
  atomic_fetch_add_explicit(&key_destructors, 1, memory_order_release);
}

static struct hermit_private_stack_v1 allocate_stack(void) {
  size_t usable = 128 * 1024;
  size_t size = usable + 8192;
  void *allocation = mmap(NULL, size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  assert(allocation != MAP_FAILED);
  assert(mprotect((char *)allocation + 4096, usable, PROT_READ | PROT_WRITE) == 0);
  return (struct hermit_private_stack_v1){
    .version = 1, .size = sizeof(struct hermit_private_stack_v1),
    .allocation_id = (uintptr_t)allocation,
    .allocation_low = (uintptr_t)allocation, .allocation_size = size,
    .usable_low = (uintptr_t)allocation + 4096, .usable_size = usable,
    .lower_guard_size = 4096, .upper_guard_size = 4096
  };
}

static void release_stack(const struct hermit_private_stack_v1 *stack) {
  assert(munmap((void *)stack->allocation_low, stack->allocation_size) == 0);
}

static void native_body(struct pl_child *child) {
  assert(child == control_child && child->self == child);
  assert(pl_child_test_capture_ok(child) == 1);
  assert(atomic_load_explicit(&child->parent_arrived, memory_order_acquire) == 1);
  assert(atomic_load_explicit(&child->entry_release, memory_order_acquire) == 1);
  assert(atomic_load_explicit(&child->pidfd_ready, memory_order_acquire) == 1);
  assert(THREAD_SELF == (struct pthread *)child->private_fs);
  if (selected("native-signal-boundaries")) {
    assert(atomic_load_explicit(&signal_usr1, memory_order_acquire) == 1);
    assert(atomic_load_explicit(&signal_usr2, memory_order_acquire) == 1);
    assert(atomic_load_explicit(&signal_bad_tls, memory_order_acquire) == 0);
  }
  assert(THREAD_SELF->tid == (int)control_raw(__NR_gettid, 0, 0, 0, 0));
  uint64_t context_token = pl_context_enter_control(&child->tls);
  assert(context_token == 1);
  pl_context_leave_control(&child->tls, context_token, 0);
  assert(THREAD_SELF == (struct pthread *)child->private_fs);
  pthread_attr_t attributes;
  void *stack_low = NULL;
  size_t stack_size = 0, guard_size = SIZE_MAX;
  assert(pthread_getattr_np(pthread_self(), &attributes) == 0);
  assert(pthread_attr_getstack(&attributes, &stack_low, &stack_size) == 0);
  assert(pthread_attr_getguardsize(&attributes, &guard_size) == 0);
  assert((uintptr_t)stack_low == child->information.stack.storage.usable_low);
  assert(stack_size == child->information.stack.storage.usable_size && guard_size == 0);
  assert(pthread_attr_destroy(&attributes) == 0);
  assert(thread_value == UINT64_C(0xa63b716ec5429d08));
  thread_value = UINT64_C(0x317dd83d72b6054e);
  errno = 137;
  assert(pthread_mutex_lock(&control_mutex) == 0);
  void *allocation = malloc(513);
  assert(allocation != NULL);
  memset(allocation, 0x91, 513);
  free(allocation);
  assert(pthread_mutex_unlock(&control_mutex) == 0);
  assert(errno == 137);
  assert(pthread_setspecific(control_key, child) == 0);
  assert(__cxa_thread_atexit_impl(tls_destructor, child, &__dso_handle) == 0);
  unsigned char bytes[32];
  assert(getrandom(bytes, sizeof bytes, 0) == (ssize_t)sizeof bytes);
  atomic_store_explicit(&rng_allocated, THREAD_SELF->getrandom_buf != NULL, memory_order_release);
  struct pl_child_borrow borrow = {0};
  assert(pl_child_borrow_acquire(child, &borrow) == 0);
  struct pl_child_borrow copied = borrow;
  assert(pl_child_borrow_release(&copied) == -EINVAL);
  if (!selected("native-held-borrow")) assert(pl_child_borrow_release(&borrow) == 0);
  atomic_store_explicit(&ran_body, 1, memory_order_release);
  if (selected("native-abandon")) pl_child_abandon(child, -ECANCELED);
  if (selected("native-held-borrow")) pl_child_retire(child);
}

int pe_private_prepare_runtime(const struct pm_crt_context *context) {
  int prepared = pl_prepare_private_tls(context);
  if (prepared != 0) return prepared;
  if (pl_gnu_root(context).code != 0 || pl_gnu_check(context).code != 0 ||
      pl_gnu_close(context).code != 0) return -EPROTO;
  control_root = context;
  return 0;
}

void pe_private_handoff_to_interpreter(const struct pm_crt_context *context) {
  assert(context == control_root && pl_is_private_owner(context) == 0);
  const struct ps_handoff *handoff = context->stack_handoff;
  assert(handoff->original_argc == 2);
  char *const *argv = (char *const *)handoff->original_argv;
  control_case = argv[1];
  assert(strcmp(control_case, getenv("PL_NATIVE_CONTROL_CASE")) == 0);
  assert(selected("native-basic") || selected("native-parent-first") ||
         selected("native-child-first") || selected("native-held-borrow") ||
         selected("native-cleanup-reentry") || selected("native-abandon") ||
         selected("native-kernel-error") || selected("native-pidfd-error") ||
         selected("native-reused-process-pidfd") ||
         selected("native-signal-boundaries"));
  struct rlimit core = {0, 0};
  assert(setrlimit(RLIMIT_CORE, &core) == 0);
  struct { uintptr_t handler, flags, restorer; uint64_t mask; } action;
  for (int number = 1; number <= 64; ++number) {
    memset(&action, 0, sizeof action);
    assert(control_raw(__NR_rt_sigaction, number, 0, (long)&action, 8) == 0);
    assert(action.handler == 0 || action.handler == 1);
  }
  if (selected("native-signal-boundaries")) install_signal_controls();
  assert(pthread_key_create(&control_key, key_destructor) == 0);
  control_child = aligned_alloc(64, sizeof(struct pl_child));
  assert(control_child != NULL);
  memset(control_child, 0, sizeof *control_child);
  struct hermit_private_stack_v1 private_stack = allocate_stack();
  struct hermit_private_stack_v1 bootstrap = allocate_stack();
  struct hermit_private_stack_v1 terminal = allocate_stack();
  private_stack.allocation_id = 0;
  struct hermit_private_reservation_owner_v1 origin = {
    .version = 1, .size = sizeof origin,
    .invocation = (uintptr_t)control_child, .entry_generation = 1
  };
  struct hermit_private_reservation_v1 reservation = {0};
  assert(pl_gnu_reserve(context, &origin, &private_stack, &reservation).code == 0);
  struct pm_crt_context wrong_root = *context;
  struct pl_child_creation refused = pl_child_start(&wrong_root, &origin, &reservation,
      &bootstrap, &terminal, control_child, native_body);
  assert(refused.kernel_entered == 0 && refused.preparation_error == -EPERM);
  assert(control_child->self == NULL && control_child->native.ticket == 0);
  if (selected("native-kernel-error")) {
    struct rlimit processes = {0, 0};
    assert(setrlimit(RLIMIT_NPROC, &processes) == 0);
  }
  if (selected("native-pidfd-error")) {
    struct rlimit descriptors = {0, 0};
    assert(setrlimit(RLIMIT_NOFILE, &descriptors) == 0);
  }
  uint64_t parent_mask_before_create = 0, parent_mask_after_create = 0;
  assert(control_raw(__NR_rt_sigprocmask, SIG_SETMASK, 0,
                     (long)&parent_mask_before_create,
                     sizeof parent_mask_before_create) == 0);
  errno = 93;
  struct pl_child_creation creation = pl_child_start(context, &origin, &reservation,
      &bootstrap, &terminal, control_child, native_body);
  assert(control_raw(__NR_rt_sigprocmask, SIG_SETMASK, 0,
                     (long)&parent_mask_after_create,
                     sizeof parent_mask_after_create) == 0);
  assert(parent_mask_after_create == parent_mask_before_create);
  fprintf(stderr, "native create: entered=%u raw=%ld preparation=%d completion=%d\n",
      creation.kernel_entered, (long)creation.kernel_result,
      creation.preparation_error, creation.completion_error);
  assert(creation.preparation_error == 0 && creation.completion_error == 0);
  assert(creation.kernel_entered == 1);
  if (selected("native-kernel-error")) {
    assert(creation.kernel_result == -EAGAIN);
    assert(atomic_load_explicit(&control_child->kernel_tid, memory_order_acquire) == 0);
    assert(atomic_load_explicit(&control_child->parent_tid, memory_order_acquire) == 0);
    assert(pl_gnu_cancel_reservation(context, &origin, &reservation).code == 0);
    assert(pl_gnu_cancel_reservation(context, &origin, &reservation).code != 0);
    release_stack(&bootstrap);
    release_stack(&terminal);
    release_stack(&private_stack);
    free(control_child);
    fputs("native actual kernel EAGAIN: one cancellation, no child\n", stderr);
    (void)control_raw(__NR_exit_group, 0, 0, 0, 0);
    __builtin_trap();
  }
  assert(creation.kernel_result > 0);
  if (selected("native-pidfd-error")) {
    assert(pl_child_wait_arrival(control_child) == -EOWNERDEAD);
    assert(control_child->entry.raw_error == -EMFILE);
    assert(atomic_load_explicit(&ran_body, memory_order_acquire) == 0);
    fputs("native actual child pidfd EMFILE after create: retained, no rollback\n", stderr);
    (void)control_raw(__NR_exit_group, 0, 0, 0, 0);
    __builtin_trap();
  }
  if (selected("native-signal-boundaries")) {
    uint64_t blocked = signal_bit(SIGUSR1) | signal_bit(SIGUSR2);
    assert(control_raw(__NR_rt_sigprocmask, SIG_BLOCK, (long)&blocked,
                       (long)&signal_unblocked_mask, sizeof blocked) == 0);
    assert(signal_unblocked_mask == control_child->inherited_signal_mask);
    for (;;) {
      uint32_t event = atomic_load_explicit(&control_child->event, memory_order_acquire);
      if (atomic_load_explicit(&signal_attach_ready, memory_order_acquire) == 1) break;
      wait_event(control_child, event);
    }
    long tgid = control_raw(__NR_getpid, 0, 0, 0, 0);
    long tid = atomic_load_explicit(&control_child->kernel_tid, memory_order_acquire);
    assert(control_raw(__NR_kill, tgid, SIGUSR1, 0, 0) == 0);
    assert(control_raw(__NR_tgkill, tgid, tid, SIGUSR2, 0) == 0);
    assert(atomic_load_explicit(&signal_usr1, memory_order_acquire) == 0);
    assert(atomic_load_explicit(&signal_usr2, memory_order_acquire) == 0);
    assert(atomic_load_explicit(&signal_bad_tls, memory_order_acquire) == 0);
    signal_boundary_release(&signal_attach_release);
  }
  assert(pl_child_wait_arrival(control_child) == 0);
  struct pl_child *copied_child = aligned_alloc(64, sizeof *copied_child);
  assert(copied_child != NULL);
  memset(copied_child, 0, sizeof *copied_child);
  copied_child->self = control_child->self;
  copied_child->root = control_child->root;
  copied_child->origin = control_child->origin;
  copied_child->native = control_child->native;
  assert(pl_child_release_entry(copied_child) == -EINVAL);
  assert(pl_child_reap(copied_child) == -EINVAL);
  free(copied_child);
  assert(atomic_load_explicit(&ran_body, memory_order_acquire) == 0);
  assert(pl_child_reap(control_child) == -EAGAIN);
  assert(pl_child_release_entry(control_child) == 0);
  assert(pl_child_release_entry(control_child) != 0);
  if (selected("native-signal-boundaries")) {
    for (;;) {
      uint32_t event = atomic_load_explicit(&control_child->event, memory_order_acquire);
      if (atomic_load_explicit(&signal_retire_ready, memory_order_acquire) == 1) break;
      wait_event(control_child, event);
    }
    assert(atomic_load_explicit(&signal_usr1, memory_order_acquire) == 1);
    assert(atomic_load_explicit(&signal_usr2, memory_order_acquire) == 1);
    assert(atomic_load_explicit(&signal_bad_tls, memory_order_acquire) == 0);
    long tgid = control_raw(__NR_getpid, 0, 0, 0, 0);
    long tid = atomic_load_explicit(&control_child->kernel_tid, memory_order_acquire);
    assert(control_raw(__NR_kill, tgid, SIGUSR1, 0, 0) == 0);
    assert(control_raw(__NR_tgkill, tgid, tid, SIGUSR2, 0) == 0);
    assert(atomic_load_explicit(&signal_usr1, memory_order_acquire) == 1);
    assert(atomic_load_explicit(&signal_usr2, memory_order_acquire) == 1);
    signal_boundary_release(&signal_retire_release);
  }
  if (selected("native-held-borrow") || selected("native-cleanup-reentry") || selected("native-abandon")) {
    for (;;) {
      uint32_t event = atomic_load_explicit(&control_child->event, memory_order_acquire);
      if (atomic_load_explicit(&control_child->stage, memory_order_acquire) == PL_CHILD_POISONED) break;
      wait_event(control_child, event);
    }
    assert(atomic_load_explicit(&ran_body, memory_order_acquire) == 1);
    assert(pl_child_wait_arrival(control_child) == -EOWNERDEAD);
    fputs("native expected refusal: retained child/storage; whole-control termination\n", stderr);
    (void)control_raw(__NR_exit_group, 0, 0, 0, 0);
    __builtin_trap();
  }
  wait_stage(control_child, PL_CHILD_RETIRED);
  assert(atomic_load_explicit(&tls_destructors, memory_order_acquire) == 1);
  assert(atomic_load_explicit(&key_destructors, memory_order_acquire) == 1);
  assert(pl_child_reap(control_child) == -EAGAIN);
  assert(atomic_load_explicit(&control_child->kernel_tid, memory_order_acquire) > 0);
  assert(thread_value == UINT64_C(0xa63b716ec5429d08));
  assert(errno == 93);
  if (selected("native-reused-process-pidfd")) {
    int child_fd = control_child->pidfd;
    long process_fd = control_raw(__NR_pidfd_open,
        control_raw(__NR_getpid, 0, 0, 0, 0), 0, 0, 0);
    assert(process_fd >= 0 && process_fd != child_fd);
    assert(control_raw(__NR_close, child_fd, 0, 0, 0) == 0);
    assert(control_raw(__NR_dup2, process_fd, child_fd, 0, 0) == child_fd);
    assert(pl_child_reap(control_child) == -ESTALE);
    assert(atomic_load_explicit(&control_child->stage, memory_order_acquire) == PL_CHILD_RETIRED);
    fputs("native reused process pidfd refused: retained, no normal reclamation\n", stderr);
    (void)control_raw(__NR_exit_group, 0, 0, 0, 0);
    __builtin_trap();
  }
  assert(pl_child_release_exit(control_child) == 0);
  for (;;) {
    int reaped = pl_child_reap(control_child);
    if (reaped == 0) break;
    assert(reaped == -EAGAIN);
    struct pollfd wait = {.fd = control_child->pidfd, .events = POLLIN};
    long ready = control_raw(__NR_poll, (long)&wait, 1, -1, 0);
    assert(ready == 1 || ready == -EINTR);
  }
  assert(pl_child_reap(control_child) == -EALREADY);
  assert(atomic_load_explicit(&control_child->kernel_tid, memory_order_acquire) == 0);
  if (selected("native-signal-boundaries")) {
    assert(control_raw(__NR_rt_sigprocmask, SIG_SETMASK, (long)&signal_unblocked_mask,
                       0, sizeof signal_unblocked_mask) == 0);
    assert(atomic_load_explicit(&signal_usr1, memory_order_acquire) == 2);
    assert(atomic_load_explicit(&signal_usr2, memory_order_acquire) == 1);
    assert(atomic_load_explicit(&signal_bad_tls, memory_order_acquire) == 0);
    assert(control_raw(__NR_rt_sigprocmask, SIG_SETMASK, (long)&signal_original_mask,
                       0, sizeof signal_original_mask) == 0);
  }
  assert(pthread_key_delete(control_key) == 0);
  release_stack(&bootstrap);
  release_stack(&terminal);
  release_stack(&private_stack);
  free(control_child);
  fprintf(stderr, "native complete: actual root/child; destructors=1/1; getrandom-buffer=%u; physical-reap\n",
      atomic_load_explicit(&rng_allocated, memory_order_acquire));
  (void)control_raw(__NR_exit_group, 0, 0, 0, 0);
  __builtin_trap();
}
