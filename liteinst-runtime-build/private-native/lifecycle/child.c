#define _GNU_SOURCE
#include "child.h"
#include <asm/prctl.h>
#include <asm/unistd.h>
#include <errno.h>
#include <limits.h>
#include <linux/futex.h>
#include <linux/pidfd.h>
#include <linux/sched.h>
#include <poll.h>
#include <string.h>
#include <sys/utsname.h>

#if defined PL_NATIVE_CET_ABI_UNAVAILABLE && PL_NATIVE_CET_ABI_UNAVAILABLE == 1
# if !defined PL_NATIVE_CET_KERNEL_RELEASE
#  error "unavailable CET ABI requires the sealed deployment kernel release"
# endif
#endif

extern long reverie_preload_trusted_syscall(long, long, long, long, long, long, long)
  __attribute__((visibility("hidden")));

_Static_assert(PL_CHILD_CLONE_FLAGS == (CLONE_VM | CLONE_FS | CLONE_FILES |
  CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM | CLONE_PARENT_SETTID |
  CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID), "one native clone profile");

static long raw(long number, long first, long second, long third,
                long fourth, long fifth) {
  return reverie_preload_trusted_syscall(number, first, second, third, fourth, fifth, 0);
}

static void wake(struct pl_child *child) {
  atomic_fetch_add_explicit(&child->event, 1, memory_order_release);
  (void)raw(__NR_futex, (long)&child->event, FUTEX_WAKE_PRIVATE, INT_MAX, 0, 0);
}

static int root_owner(const struct pl_child *child) {
  if (child == NULL || child->self != child || child->native.control != (uintptr_t)child)
    return -EINVAL;
  return pl_gnu_child_authorize(child->root, &child->origin).code == 0 ? 0 : -EPERM;
}

static int child_owner(const struct pl_child *child) {
  if (child == NULL || child->self != child || child->native.control != (uintptr_t)child)
    return -EINVAL;
  long tid = raw(__NR_gettid, 0, 0, 0, 0, 0);
  uint64_t fs = 0, gs = 0;
  if (tid <= 0 || tid != (long)atomic_load_explicit(&child->kernel_tid, memory_order_acquire) ||
      raw(__NR_arch_prctl, ARCH_GET_FS, (long)&fs, 0, 0, 0) != 0 ||
      raw(__NR_arch_prctl, ARCH_GET_GS, (long)&gs, 0, 0, 0) != 0 ||
      fs != child->private_fs || gs != child->inherited_gs)
    return -EPERM;
  return 0;
}

static int disjoint(uintptr_t begin, size_t size, uintptr_t other, size_t length) {
  return begin != 0 && other != 0 && size != 0 && length != 0 &&
    size <= UINTPTR_MAX - begin && length <= UINTPTR_MAX - other &&
    (begin + size <= other || other + length <= begin);
}

static int stack_valid(const struct hermit_private_stack_v1 *stack) {
  return stack != NULL && stack->version == 1 && stack->size == sizeof *stack &&
    stack->allocation_id != 0 && stack->reserved[0] == 0 && stack->reserved[1] == 0 &&
    stack->allocation_low != 0 && stack->allocation_low % 4096 == 0 &&
    stack->allocation_size <= UINTPTR_MAX - stack->allocation_low &&
    stack->lower_guard_size == 4096 && stack->upper_guard_size == 4096 &&
    stack->usable_size >= 65536 && stack->usable_size % 4096 == 0 &&
    stack->usable_size <= SIZE_MAX - 8192 &&
    stack->allocation_size == stack->usable_size + 8192 &&
    stack->usable_low == stack->allocation_low + 4096;
}

static int separate_stack(const struct hermit_private_stack_v1 *left,
                          const struct hermit_private_stack_v1 *right) {
  return left->allocation_id != right->allocation_id &&
    disjoint(left->allocation_low, left->allocation_size,
             right->allocation_low, right->allocation_size);
}

static int cet_before_create(struct pl_child *child) {
  child->parent_shstk = UINT64_MAX;
  long result = raw(__NR_arch_prctl, 0x5005, (long)&child->parent_shstk, 0, 0, 0);
  child->parent_shstk_result = result;
  if (result == 0 && child->parent_shstk == 0) return 0;
#if defined PL_NATIVE_CET_ABI_UNAVAILABLE && PL_NATIVE_CET_ABI_UNAVAILABLE == 1
  if (result == -EINVAL) {
    struct utsname observed = {0};
    if (raw(__NR_uname, (long)&observed, 0, 0, 0, 0) == 0 &&
        strcmp(observed.release, PL_NATIVE_CET_KERNEL_RELEASE) == 0) return 0;
  }
#endif
  return -ENOTSUP;
}

static struct pl_child_creation preparation_failure(int error) {
  return (struct pl_child_creation){.preparation_error = error};
}

struct pl_child_creation pl_child_start(const struct pm_crt_context *root,
    const struct hermit_private_reservation_owner_v1 *origin,
    const struct hermit_private_reservation_v1 *reservation,
    const struct hermit_private_stack_v1 *bootstrap,
    const struct hermit_private_stack_v1 *terminal,
    struct pl_child *child, void (*body)(struct pl_child *)) {
  if (pl_gnu_child_authorize(root, origin).code != 0) return preparation_failure(-EPERM);
  if (child == NULL || (uintptr_t)child % 64 != 0 || body == NULL ||
      (uintptr_t)body < root->runtime_bias ||
      (uintptr_t)body - root->runtime_bias >= root->runtime_span ||
      !stack_valid(bootstrap) || !stack_valid(terminal) ||
      !separate_stack(bootstrap, terminal)) return preparation_failure(-EINVAL);
  const unsigned char *bytes = (const unsigned char *)child;
  for (size_t index = 0; index < sizeof *child; ++index)
    if (bytes[index] != 0) return preparation_failure(-EINVAL);
  struct hermit_private_stack_info_v1 info = {0};
  if (pl_gnu_reservation_query(root, origin, reservation, &info).code != 0)
    return preparation_failure(-EINVAL);
  if (!separate_stack(bootstrap, &info.storage) || !separate_stack(terminal, &info.storage) ||
      !disjoint((uintptr_t)child, sizeof *child, bootstrap->allocation_low, bootstrap->allocation_size) ||
      !disjoint((uintptr_t)child, sizeof *child, terminal->allocation_low, terminal->allocation_size) ||
      !disjoint((uintptr_t)child, sizeof *child, info.storage.allocation_low, info.storage.allocation_size) ||
      pl_destination_disjoint(root->owner, bootstrap->allocation_low, bootstrap->allocation_size) != 0 ||
      pl_destination_disjoint(root->owner, terminal->allocation_low, terminal->allocation_size) != 0)
    return preparation_failure(-EINVAL);
  child->root = root;
  child->origin = *origin;
  child->reservation = *reservation;
  child->bootstrap = *bootstrap;
  child->terminal = *terminal;
  child->body = body;
  child->pidfd = -1;
  child->boot_low = bootstrap->usable_low;
  child->boot_high = bootstrap->usable_low + bootstrap->usable_size;
  child->terminal_top = terminal->usable_low + terminal->usable_size;
  child->private_top = info.storage.usable_low + info.storage.usable_size;
  child->inherited_fs = atomic_load_explicit(&pl_tls.private_fs, memory_order_relaxed);
  child->inherited_gs = atomic_load_explicit(&pl_tls.private_gs, memory_order_relaxed);
  child->exit_fs = atomic_load_explicit(&pl_tls.guest_fs, memory_order_relaxed);
  child->exit_gs = atomic_load_explicit(&pl_tls.guest_gs, memory_order_relaxed);
  int cet = cet_before_create(child);
  if (cet != 0) return preparation_failure(cet);
  if (__hermit_private_native_begin_v1(origin, reservation, (uintptr_t)child,
        &child->native, &child->information) != HERMIT_PRIVATE_OK)
    return preparation_failure(-EPROTO);
  child->self = child;
  child->private_fs = child->information.descriptor;
  uintptr_t *slot = (uintptr_t *)child->boot_high;
  slot[-2] = (uintptr_t)pl_child_entry;
  slot[-1] = (uintptr_t)child;
  atomic_store_explicit(&child->stage, PL_CHILD_STARTING, memory_order_release);
  long result = pl_child_kernel(child);
  struct pl_child_creation creation = {.kernel_result = result, .kernel_entered = 1};
  atomic_store_explicit(&child->raw_create, result, memory_order_release);
  if (result < 0 && result >= -4095) {
    if (atomic_load_explicit(&child->stage, memory_order_acquire) != PL_CHILD_STARTING ||
        atomic_load_explicit(&child->kernel_tid, memory_order_acquire) != 0 ||
        atomic_load_explicit(&child->parent_tid, memory_order_acquire) != 0 ||
        __hermit_private_native_kernel_error_v1(origin, &child->native, result) != HERMIT_PRIVATE_OK) {
      (void)pl_gnu_child_poison(root, origin);
      creation.completion_error = -EOWNERDEAD;
      return creation;
    }
    atomic_store_explicit(&child->failure, result, memory_order_release);
    return creation;
  }
  if (result <= 0 || result > INT_MAX ||
      result != (long)atomic_load_explicit(&child->parent_tid, memory_order_acquire)) {
    (void)pl_gnu_child_poison(root, origin);
    creation.completion_error = -EOWNERDEAD;
    return creation;
  }
  atomic_store_explicit(&child->parent_arrived, 1, memory_order_release);
  wake(child);
  return creation;
}

int pl_child_wait_arrival(struct pl_child *child) {
  int status = root_owner(child);
  if (status != 0) return status;
  for (;;) {
    uint32_t event = atomic_load_explicit(&child->event, memory_order_acquire);
    uint32_t stage = atomic_load_explicit(&child->stage, memory_order_acquire);
    if (stage == PL_CHILD_POISONED) {
      (void)pl_gnu_child_poison(child->root, &child->origin);
      return -EOWNERDEAD;
    }
    if (atomic_load_explicit(&child->parent_arrived, memory_order_acquire) != 0 &&
        stage >= PL_CHILD_ACTIVE && stage <= PL_CHILD_EXITING)
      return 0;
    if (atomic_load_explicit(&child->raw_create, memory_order_acquire) < 0) return -ECHILD;
    long result = raw(__NR_futex, (long)&child->event, FUTEX_WAIT_PRIVATE, event, 0, 0);
    if (result != 0 && result != -EAGAIN && result != -EINTR) return (int)result;
  }
}

int pl_child_release_entry(struct pl_child *child) {
  int status = root_owner(child);
  if (status != 0) return status;
  int64_t created = atomic_load_explicit(&child->raw_create, memory_order_acquire);
  if (created <= 0 || created > INT_MAX ||
      atomic_load_explicit(&child->parent_arrived, memory_order_acquire) != 1 ||
      created != atomic_load_explicit(&child->parent_tid, memory_order_acquire) ||
      created != atomic_load_explicit(&child->kernel_tid, memory_order_acquire) ||
      atomic_load_explicit(&child->pidfd_ready, memory_order_acquire) != 1 ||
      atomic_load_explicit(&child->stage, memory_order_acquire) != PL_CHILD_ACTIVE)
    return -EBUSY;
  uint32_t empty = 0;
  if (!atomic_compare_exchange_strong_explicit(&child->entry_release, &empty, 1,
        memory_order_acq_rel, memory_order_acquire)) return -EALREADY;
  wake(child);
  return 0;
}

void pl_child_arrived(struct pl_child *child) {
  if (child_owner(child) != 0 || child->entry.fs_base != child->inherited_fs ||
      child->entry.gs_base != child->inherited_gs)
    pl_child_abandon(child, -EPERM);
  long tid = raw(__NR_gettid, 0, 0, 0, 0, 0);
  long fd = raw(__NR_pidfd_open, tid, PIDFD_THREAD, 0, 0, 0);
  if (fd < 0 || fd > INT_MAX) pl_child_abandon(child, fd);
  child->pidfd = (int)fd;
  struct pidfd_info info = {.mask = PIDFD_INFO_PID};
  if (raw(__NR_ioctl, fd, PIDFD_GET_INFO, (long)&info, 0, 0) != 0 ||
      !(info.mask & PIDFD_INFO_PID) || info.pid != (uint32_t)tid ||
      info.tgid != (uint32_t)raw(__NR_getpid, 0, 0, 0, 0, 0) || info.pid == info.tgid ||
      raw(__NR_fstat, fd, (long)&child->pidfd_stat, 0, 0, 0) != 0)
    pl_child_abandon(child, -EPROTO);
  atomic_store_explicit(&child->pidfd_ready, 1, memory_order_release);
  atomic_store_explicit(&child->stage, PL_CHILD_ARRIVED, memory_order_release);
  wake(child);
  if (__hermit_private_native_attach_v1(&child->native) != HERMIT_PRIVATE_OK)
    pl_child_abandon(child, -EPROTO);
  atomic_store_explicit(&child->tls.private_fs, child->private_fs, memory_order_relaxed);
  atomic_store_explicit(&child->tls.owner_tid, (uint64_t)tid, memory_order_relaxed);
  atomic_store_explicit(&child->tls.guest_fs, child->exit_fs, memory_order_relaxed);
  atomic_store_explicit(&child->tls.private_gs, child->inherited_gs, memory_order_relaxed);
  atomic_store_explicit(&child->tls.guest_gs, child->exit_gs, memory_order_relaxed);
  atomic_store_explicit(&child->tls.phase, 2, memory_order_release);
  atomic_store_explicit(&child->stage, PL_CHILD_ACTIVE, memory_order_release);
  wake(child);
  for (;;) {
    uint32_t event = atomic_load_explicit(&child->event, memory_order_acquire);
    if (atomic_load_explicit(&child->entry_release, memory_order_acquire) == 1) break;
    long result = raw(__NR_futex, (long)&child->event, FUTEX_WAIT_PRIVATE, event, 0, 0);
    if (result != 0 && result != -EAGAIN && result != -EINTR)
      pl_child_abandon(child, result);
  }
  child->body(child);
  pl_child_retire(child);
}

int pl_child_borrow_acquire(struct pl_child *child, struct pl_child_borrow *borrow) {
  if (child_owner(child) != 0 || borrow == NULL || borrow->child != NULL ||
      atomic_load_explicit(&child->stage, memory_order_acquire) != PL_CHILD_ACTIVE)
    return -EPERM;
  uintptr_t empty = 0;
  if (!atomic_compare_exchange_strong_explicit(&child->borrow, &empty, (uintptr_t)borrow,
        memory_order_acq_rel, memory_order_acquire)) return -EBUSY;
  borrow->child = child;
  return 0;
}

int pl_child_borrow_release(struct pl_child_borrow *borrow) {
  if (borrow == NULL || child_owner(borrow->child) != 0) return -EPERM;
  uintptr_t expected = (uintptr_t)borrow;
  if (!atomic_compare_exchange_strong_explicit(&borrow->child->borrow, &expected, 0,
        memory_order_acq_rel, memory_order_acquire)) return -EINVAL;
  borrow->child = NULL;
  return 0;
}

void pl_child_retire(struct pl_child *child) {
  if (child_owner(child) != 0 ||
      atomic_load_explicit(&child->borrow, memory_order_acquire) != 0 ||
      atomic_load_explicit(&child->tls.deferred, memory_order_acquire) != 0)
    pl_child_abandon(child, -EBUSY);
  uint32_t active = PL_CHILD_ACTIVE;
  if (!atomic_compare_exchange_strong_explicit(&child->stage, &active, PL_CHILD_RETIRING,
        memory_order_acq_rel, memory_order_acquire)) pl_child_abandon(child, -EALREADY);
  if (__hermit_private_native_retire_v1(&child->native) != HERMIT_PRIVATE_OK)
    pl_child_abandon(child, -EPROTO);
  atomic_store_explicit(&child->stage, PL_CHILD_RETIRED, memory_order_release);
  wake(child);
  pl_child_terminal(child);
}

int pl_child_release_exit(struct pl_child *child) {
  int status = root_owner(child);
  if (status != 0) return status;
  if (atomic_load_explicit(&child->stage, memory_order_acquire) != PL_CHILD_RETIRED)
    return -EBUSY;
  uint32_t empty = 0;
  if (!atomic_compare_exchange_strong_explicit(&child->exit_release, &empty, 1,
        memory_order_acq_rel, memory_order_acquire)) return -EALREADY;
  long result = raw(__NR_futex, (long)&child->exit_release, FUTEX_WAKE_PRIVATE, 1, 0, 0);
  return result < 0 ? (int)result : 0;
}

int pl_child_reap(struct pl_child *child) {
  int status = root_owner(child);
  if (status != 0) return status;
  uint32_t stage = atomic_load_explicit(&child->stage, memory_order_acquire);
  if (stage == PL_CHILD_REAPED) return -EALREADY;
  if (atomic_load_explicit(&child->pidfd_ready, memory_order_acquire) != 1) return -EAGAIN;
  struct stat observed = {0};
  if (raw(__NR_fstat, child->pidfd, (long)&observed, 0, 0, 0) != 0 ||
      observed.st_dev != child->pidfd_stat.st_dev || observed.st_ino != child->pidfd_stat.st_ino ||
      observed.st_mode != child->pidfd_stat.st_mode) return -ESTALE;
  struct pollfd descriptor = {.fd = child->pidfd, .events = POLLIN};
  long ready = raw(__NR_poll, (long)&descriptor, 1, 0, 0, 0);
  if (ready == 0) return -EAGAIN;
  if (ready != 1 || !(descriptor.revents & POLLIN) ||
      (descriptor.revents & ~(POLLIN | POLLHUP)) != 0) return -EIO;
  stage = atomic_load_explicit(&child->stage, memory_order_acquire);
  if (stage != PL_CHILD_EXITING || atomic_load_explicit(&child->borrow, memory_order_acquire) != 0 ||
      atomic_load_explicit(&child->kernel_tid, memory_order_acquire) != 0) {
    (void)pl_gnu_child_poison(child->root, &child->origin);
    return -EOWNERDEAD;
  }
  if (__hermit_private_native_destroy_v1(&child->origin, &child->native) != HERMIT_PRIVATE_OK) {
    (void)pl_gnu_child_poison(child->root, &child->origin);
    return -EOWNERDEAD;
  }
  long closed = raw(__NR_close, child->pidfd, 0, 0, 0, 0);
  child->pidfd = -1;
  if (closed != 0) {
    atomic_store_explicit(&child->stage, PL_CHILD_POISONED, memory_order_release);
    (void)pl_gnu_child_poison(child->root, &child->origin);
    return -EIO;
  }
  atomic_store_explicit(&child->stage, PL_CHILD_REAPED, memory_order_release);
  return 0;
}
