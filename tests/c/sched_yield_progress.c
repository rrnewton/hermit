/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Regression guest for GH #81: chaos must not starve sched_yield loops when
 * timer preemption is disabled.
 *
 * The main thread spins on an atomic flag, calling sched_yield() while it
 * waits. A worker thread does a small amount of work and then publishes a
 * value and sets the flag. Under `--chaos --max-timeslice=disabled`,
 * priorities are fixed at thread creation and only re-randomized at timer
 * preemptions, which are off. Before the fix, a spinning sched_yield loop that
 * happened to hold the highest priority would monopolize the single logical CPU
 * and the worker would never run, hanging until an external timeout. With the
 * fix, sched_yield is treated as a chaos reprioritization point, so the worker
 * eventually runs and the program makes progress.
 *
 * With timer preemption on, the same loop starved the worker under
 * `--chaos --chaos-target-races`
 * (https://github.com/rrnewton/hermit/issues/4068): the worker can start in
 * the band that runs last, and chaos redraws a band only when a slice expires,
 * which a sched_yield loop never reaches.
 * `--two-spinners` adds a second thread spinning the same way, so two yielders
 * can take turns ahead of the worker. `--yield-then-write` turns it around:
 * main yields once, makes one ordinary syscall, then sets the flag that two
 * pure spinners (no syscalls) wait for, so it is the yielder that is waited on.
 *
 * `--vfork-spin` and `--spawn-spin` start this program again as a vfork
 * child, through vfork(2) and execve(2) or through posix_spawn(3), whose
 * glibc implementation clones with CLONE_VM | CLONE_VFORK. In the new image
 * (`--spin-child`) the main thread spins without syscalls until a second
 * thread, which first sleeps for 1 ms, sets the flag. Detcore gave a vfork
 * child a persistent priority ahead of every other thread, and nothing reset
 * it at the exec, so the spinner kept the only logical CPU and the sleeper
 * never ran, even without chaos (Debian's hey build, where `go build` and
 * dh_golang start Go programs this way, hung in its compile and `go list`).
 *
 * `--vfork-child-first` checks the schedule Hermit chooses inside the vfork
 * interval: while the parent waits in vfork(2), the child runs before every
 * other runnable thread. A sibling of the parent counts without syscalls; the
 * child reads the count, makes a call that takes a scheduler turn, and reads
 * it again. Under Hermit both reads agree. Linux allows that schedule but does
 * not require it, so this mode is not expected to pass natively.
 *
 * Success is printing "sched-yield-progress-ok <value>" and exiting 0. Failure
 * is a hang (caught by the harness timeout).
 */

#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <spawn.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static atomic_int g_ready = 0;
static atomic_ullong g_value = 0;

static void* worker(void* arg) {
  (void)arg;
  /* A bit of real work so the worker cannot complete in the same turn it is
   * created; this widens the window in which the main thread would starve it.
   */
  unsigned long long acc = 0;
  for (unsigned long long i = 0; i < 2000000ULL; i++) {
    acc += i;
  }
  atomic_store_explicit(&g_value, acc | 1ULL, memory_order_seq_cst);
  atomic_store_explicit(&g_ready, 1, memory_order_release);
  return NULL;
}

/* Spin waiting for the worker, yielding the CPU on every iteration. */
static void* spinner(void* arg) {
  (void)arg;
  while (atomic_load_explicit(&g_ready, memory_order_acquire) == 0) {
    sched_yield();
  }
  return NULL;
}

/* Spin without syscalls until main publishes the flag. */
static void* silent_spinner(void* arg) {
  (void)arg;
  while (atomic_load_explicit(&g_ready, memory_order_acquire) == 0) {
  }
  return NULL;
}

static int run_yield_then_write(void) {
  pthread_t spinners[2];
  for (int i = 0; i < 2; i++) {
    if (pthread_create(&spinners[i], NULL, silent_spinner, NULL) != 0) {
      fprintf(stderr, "pthread_create failed\n");
      return 2;
    }
  }
  sched_yield();
  const char msg[] = "yielded\n";
  if (write(1, msg, strlen(msg)) < 0) {
    return 8;
  }
  atomic_store_explicit(&g_ready, 1, memory_order_release);
  for (int i = 0; i < 2; i++) {
    if (pthread_join(spinners[i], NULL) != 0) {
      fprintf(stderr, "pthread_join failed\n");
      return 3;
    }
  }
  printf("sched-yield-progress-ok yield-then-write\n");
  return 0;
}

/* Sleep briefly, then set the flag the main thread spins on. */
static void* sleeping_setter(void* arg) {
  (void)arg;
  struct timespec delay = {0, 1000000};
  nanosleep(&delay, NULL);
  atomic_store_explicit(&g_ready, 1, memory_order_release);
  return NULL;
}

/* The new image of `--vfork-spin` and `--spawn-spin`: spin without syscalls
 * until a sleeping sibling sets the flag. */
static int run_spin_child(void) {
  pthread_t setter;
  if (pthread_create(&setter, NULL, sleeping_setter, NULL) != 0) {
    fprintf(stderr, "pthread_create failed\n");
    return 2;
  }
  silent_spinner(NULL);
  if (pthread_join(setter, NULL) != 0) {
    fprintf(stderr, "pthread_join failed\n");
    return 3;
  }
  printf("sched-yield-progress-ok spin-child\n");
  return 0;
}

/* Wait for `child`, require a clean exit, and print `marker`. */
static int wait_for_child(pid_t child, const char* marker) {
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 6;
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    fprintf(stderr, "child failed: status=%d\n", status);
    return 7;
  }
  printf("%s\n", marker);
  return 0;
}

static int run_vfork_spin(const char* self) {
  pid_t child = vfork();
  if (child < 0) {
    perror("vfork");
    return 5;
  }
  if (child == 0) {
    execl(self, self, "--spin-child", NULL);
    _exit(127);
  }
  return wait_for_child(child, "vfork-spin-progress-ok");
}

static int run_spawn_spin(const char* self) {
  char* child_argv[] = {(char*)self, "--spin-child", NULL};
  pid_t child;
  int error = posix_spawn(&child, self, NULL, NULL, child_argv, environ);
  if (error != 0) {
    fprintf(stderr, "posix_spawn failed: %s\n", strerror(error));
    return 5;
  }
  return wait_for_child(child, "spawn-spin-progress-ok");
}

/* What the counting sibling shares with run_vfork_child_first. */
struct counter_state {
  atomic_ullong count;
  atomic_int stop;
};

/* Count without syscalls until told to stop. */
static void* counter(void* arg) {
  struct counter_state* state = arg;
  while (atomic_load_explicit(&state->stop, memory_order_relaxed) == 0) {
    atomic_fetch_add_explicit(&state->count, 1, memory_order_relaxed);
  }
  return NULL;
}

static int run_vfork_child_first(void) {
  /* The sibling writes to this frame, so every return after pthread_create
   * first stops the sibling and joins it. */
  struct counter_state state = {0, 0};
  pthread_t sibling;
  if (pthread_create(&sibling, NULL, counter, &state) != 0) {
    fprintf(stderr, "pthread_create failed\n");
    return 2;
  }
  /* The sibling has had a turn, so it is a runnable thread of the run. */
  while (atomic_load_explicit(&state.count, memory_order_relaxed) == 0) {
    sched_yield();
  }
  pid_t child = vfork();
  if (child < 0) {
    perror("vfork");
    atomic_store_explicit(&state.stop, 1, memory_order_relaxed);
    (void)pthread_join(sibling, NULL);
    return 5;
  }
  if (child == 0) {
    unsigned long long before =
        atomic_load_explicit(&state.count, memory_order_relaxed);
    /* A path lookup takes a scheduler turn; it finds nothing. */
    int fd = open("/sched-yield-progress-vfork-child-first", O_RDONLY);
    if (fd >= 0) {
      close(fd);
    }
    unsigned long long after =
        atomic_load_explicit(&state.count, memory_order_relaxed);
    _exit(before == after ? 0 : 1);
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    atomic_store_explicit(&state.stop, 1, memory_order_relaxed);
    (void)pthread_join(sibling, NULL);
    return 6;
  }
  atomic_store_explicit(&state.stop, 1, memory_order_relaxed);
  if (pthread_join(sibling, NULL) != 0) {
    fprintf(stderr, "pthread_join failed\n");
    return 3;
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    printf("vfork-child-first lost: a sibling ran in the vfork interval\n");
    return 9;
  }
  printf("sched-yield-progress-ok vfork-child-first\n");
  return 0;
}

static int run_progress(int two_spinners) {
  pthread_t second;
  if (two_spinners && pthread_create(&second, NULL, spinner, NULL) != 0) {
    fprintf(stderr, "pthread_create failed\n");
    return 2;
  }
  pthread_t t;
  if (pthread_create(&t, NULL, worker, NULL) != 0) {
    fprintf(stderr, "pthread_create failed\n");
    return 2;
  }

  spinner(NULL);

  if (pthread_join(t, NULL) != 0 ||
      (two_spinners && pthread_join(second, NULL) != 0)) {
    fprintf(stderr, "pthread_join failed\n");
    return 3;
  }

  unsigned long long value =
      atomic_load_explicit(&g_value, memory_order_seq_cst);
  printf("sched-yield-progress-ok %llu\n", value);
  return 0;
}

int main(int argc, char** argv) {
  if (argc == 1 || (argc == 2 && strcmp(argv[1], "--vfork-child") == 0)) {
    return run_progress(0);
  }
  if (argc == 2 && strcmp(argv[1], "--two-spinners") == 0) {
    return run_progress(1);
  }
  if (argc == 2 && strcmp(argv[1], "--yield-then-write") == 0) {
    return run_yield_then_write();
  }
  if (argc == 2 && strcmp(argv[1], "--spin-child") == 0) {
    return run_spin_child();
  }
  if (argc == 2 && strcmp(argv[1], "--vfork-spin") == 0) {
    return run_vfork_spin(argv[0]);
  }
  if (argc == 2 && strcmp(argv[1], "--spawn-spin") == 0) {
    return run_spawn_spin(argv[0]);
  }
  if (argc == 2 && strcmp(argv[1], "--vfork-child-first") == 0) {
    return run_vfork_child_first();
  }
  if (argc != 2 || strcmp(argv[1], "--vfork") != 0) {
    fprintf(stderr,
            "usage: %s [--vfork|--vfork-child|--two-spinners|"
            "--yield-then-write|--vfork-spin|--spawn-spin|--spin-child|"
            "--vfork-child-first]\n",
            argv[0]);
    return 4;
  }

  pid_t child = vfork();
  if (child < 0) {
    perror("vfork");
    return 5;
  }
  if (child == 0) {
    execl(argv[0], argv[0], "--vfork-child", NULL);
    _exit(127);
  }

  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 6;
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    fprintf(stderr, "vfork child failed: status=%d\n", status);
    return 7;
  }

  printf("vfork-sched-yield-progress-ok\n");
  return 0;
}
