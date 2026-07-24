/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

static pthread_barrier_t ready;
static pthread_mutex_t robust_mutex;
static _Atomic pid_t child_tid;
static _Atomic int usr1_delivered;
static _Atomic int usr2_delivered;
static _Atomic int usr2_on_child;
static _Thread_local int is_worker_thread;

static void fail_errno(const char *operation) {
  perror(operation);
  exit(1);
}

static void check_pthread(int result, const char *operation) {
  if (result != 0) {
    errno = result;
    fail_errno(operation);
  }
}

static void signal_handler(int signal_number) {
  if (signal_number == SIGUSR1) {
    atomic_fetch_add_explicit(&usr1_delivered, 1, memory_order_relaxed);
  } else if (signal_number == SIGUSR2) {
    atomic_store_explicit(&usr2_on_child, is_worker_thread, memory_order_relaxed);
    atomic_fetch_add_explicit(&usr2_delivered, 1, memory_order_relaxed);
  }
}

static void check_partial_clone3(void) {
#ifdef SYS_clone3
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size < 64) {
    fputs("invalid page size\n", stderr);
    exit(1);
  }
  void *mapping =
      mmap(NULL, (size_t)page_size * 2, PROT_READ | PROT_WRITE,
           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED) {
    fail_errno("mmap(clone3)");
  }
  unsigned char *guard = (unsigned char *)mapping + page_size;
  if (mprotect(guard, (size_t)page_size, PROT_NONE) != 0) {
    fail_errno("mprotect(clone3)");
  }
  uint64_t *short_args = (uint64_t *)(guard - sizeof(uint64_t));
  *short_args = CLONE_VM | CLONE_THREAD;
  errno = 0;
  long result = syscall(SYS_clone3, short_args, 64);
  int clone_errno = errno;
  if (result != -1 || (clone_errno != EFAULT && clone_errno != ENOSYS)) {
    fprintf(stderr, "clone3 partial read: result=%ld errno=%d\n", result,
            clone_errno);
    exit(1);
  }
  if (munmap(mapping, (size_t)page_size * 2) != 0) {
    fail_errno("munmap(clone3)");
  }
#endif
}

static void *worker(void *opaque) {
  (void)opaque;
  is_worker_thread = 1;
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  check_pthread(pthread_sigmask(SIG_UNBLOCK, &mask, NULL),
                "pthread_sigmask(SIG_UNBLOCK)");
  atomic_store_explicit(&child_tid, (pid_t)syscall(SYS_gettid),
                        memory_order_release);
  int barrier_result = pthread_barrier_wait(&ready);
  if (barrier_result != 0 && barrier_result != PTHREAD_BARRIER_SERIAL_THREAD) {
    check_pthread(barrier_result, "pthread_barrier_wait(worker)");
  }
  while (atomic_load_explicit(&usr1_delivered, memory_order_acquire) == 0) {
    sched_yield();
  }

  check_pthread(pthread_mutex_lock(&robust_mutex),
                "pthread_mutex_lock(robust)");
  check_pthread(pthread_mutex_unlock(&robust_mutex),
                "pthread_mutex_unlock(robust)");
  return NULL;
}

int main(void) {
  check_partial_clone3();

  struct sigaction action = {0};
  action.sa_handler = signal_handler;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, NULL) != 0 ||
      sigaction(SIGUSR2, &action, NULL) != 0) {
    fail_errno("sigaction");
  }

  sigset_t blocked;
  sigset_t previous;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGUSR1);
  check_pthread(pthread_sigmask(SIG_BLOCK, &blocked, &previous),
                "pthread_sigmask(parent block)");

  pthread_mutexattr_t attributes;
  check_pthread(pthread_mutexattr_init(&attributes),
                "pthread_mutexattr_init");
  check_pthread(pthread_mutexattr_setrobust(&attributes, PTHREAD_MUTEX_ROBUST),
                "pthread_mutexattr_setrobust");
  check_pthread(pthread_mutex_init(&robust_mutex, &attributes),
                "pthread_mutex_init(robust)");
  check_pthread(pthread_mutexattr_destroy(&attributes),
                "pthread_mutexattr_destroy");
  check_pthread(pthread_barrier_init(&ready, NULL, 2),
                "pthread_barrier_init");

  pthread_t thread;
  check_pthread(pthread_create(&thread, NULL, worker, NULL), "pthread_create");
  int barrier_result = pthread_barrier_wait(&ready);
  if (barrier_result != 0 && barrier_result != PTHREAD_BARRIER_SERIAL_THREAD) {
    check_pthread(barrier_result, "pthread_barrier_wait(parent)");
  }

  if (syscall(SYS_tgkill, getpid(),
              atomic_load_explicit(&child_tid, memory_order_acquire),
              SIGUSR1) != 0) {
    fail_errno("tgkill");
  }
  while (atomic_load_explicit(&usr1_delivered, memory_order_acquire) == 0) {
    sched_yield();
  }
  if (kill(getpid(), SIGUSR2) != 0) {
    fail_errno("kill");
  }
  while (atomic_load_explicit(&usr2_delivered, memory_order_acquire) == 0) {
    sched_yield();
  }

  check_pthread(pthread_join(thread, NULL), "pthread_join");
  check_pthread(pthread_sigmask(SIG_SETMASK, &previous, NULL),
                "pthread_sigmask(parent restore)");
  check_pthread(pthread_barrier_destroy(&ready), "pthread_barrier_destroy");
  check_pthread(pthread_mutex_destroy(&robust_mutex),
                "pthread_mutex_destroy(robust)");

  printf("dbi-signal-thread-ok usr1=%d usr2=%d usr2_child=%d\n",
         atomic_load_explicit(&usr1_delivered, memory_order_relaxed),
         atomic_load_explicit(&usr2_delivered, memory_order_relaxed),
         atomic_load_explicit(&usr2_on_child, memory_order_relaxed));
  return 0;
}
