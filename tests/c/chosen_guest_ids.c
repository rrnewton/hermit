/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * chosen_guest_ids: prints every process and thread ID the guest can observe
 * through the kernel, by each way it learns one, so that two runs of a guest
 * without a PID namespace (`hermit run --no-namespace`) can be compared ID for
 * ID.
 *
 * It creates, in this order: two threads (pthread_create, whose clone the C
 * library makes with CLONE_PARENT_SETTID), a process by fork, a process by
 * vfork, a process by a raw clone with CLONE_PARENT_SETTID and
 * CLONE_CHILD_SETTID, and a process by a raw clone3. For each it prints the ID
 * the creating call returned, the ID the new task reads for itself (getpid or
 * gettid), the words the kernel wrote for it (the settid words), and the ID
 * the wait for it returned (waitpid, waitid's si_pid). It prints the main
 * thread's own IDs and its /proc/self link first.
 *
 * With the argument `collide`, before the fork the guest makes a process of
 * its own with clone3's set_tid naming the ID three after its first (the ID
 * Hermit would choose next), which it needs CAP_CHECKPOINT_RESTORE or
 * CAP_SYS_ADMIN for, and keeps it alive across the fork, so that Hermit's
 * choice for the fork is taken.
 *
 * Every line is the ID's offset from the first process's, so a run whose IDs
 * are chosen from a base prints the same bytes whatever the base, with the
 * offsets in creation order, and a run on host IDs shows whatever IDs other
 * host tasks left. Exits 0 when every ID agreed with the others for the same
 * task, 1 otherwise.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/sched.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t first;
static int mismatches;

static void report(const char* what, long id, long expected) {
  printf("%s %+ld\n", what, id - first);
  if (id != expected) {
    printf("  MISMATCH: %s is %ld, expected %ld\n", what, id, expected);
    mismatches++;
  }
}

/* An ID whose value only the numbering decides: printed, not checked. */
static void show(const char* what, long id) {
  printf("%s %+ld\n", what, id - first);
}

static void* thread_main(void* arg) {
  *(pid_t*)arg = (pid_t)syscall(SYS_gettid);
  return NULL;
}

static pid_t wait_for(pid_t child, const char* what) {
  int status = 0;
  pid_t waited = waitpid(child, &status, 0);
  if (waited < 0) {
    printf("%s: waitpid failed: %s\n", what, strerror(errno));
    exit(1);
  }
  return waited;
}

/* A process with ID `id`, made by clone3's set_tid, that waits to be
 * killed. */
static pid_t hold(pid_t id) {
  struct clone_args args;
  memset(&args, 0, sizeof args);
  args.exit_signal = SIGCHLD;
  args.set_tid = (uint64_t)(uintptr_t)&id;
  args.set_tid_size = 1;
  long holder = syscall(SYS_clone3, &args, sizeof args);
  if (holder == 0) {
    for (;;) {
      pause();
    }
  }
  if (holder < 0) {
    printf("clone3 set_tid %d failed: %s\n", id, strerror(errno));
    exit(1);
  }
  return (pid_t)holder;
}

int main(int argc, char** argv) {
  int collide = argc > 1 && strcmp(argv[1], "collide") == 0;
  setvbuf(stdout, NULL, _IONBF, 0);
  first = getpid();
  printf("first process %d\n", first);
  report("main gettid", syscall(SYS_gettid), first);
  char link[32] = {0};
  if (readlink("/proc/self", link, sizeof link - 1) < 0) {
    printf("readlink /proc/self failed: %s\n", strerror(errno));
    return 1;
  }
  report("/proc/self", strtol(link, NULL, 10), first);

  for (int i = 0; i < 2; i++) {
    pthread_t thread;
    pid_t seen = 0;
    if (pthread_create(&thread, NULL, thread_main, &seen) != 0) {
      printf("pthread_create failed\n");
      return 1;
    }
    pthread_join(thread, NULL);
    /* The C library keeps the new thread's ID where CLONE_PARENT_SETTID
     * wrote it; the thread's own gettid is the kernel's answer. */
    show("thread gettid", seen);
  }

  pid_t holder = collide ? hold(first + 3) : 0;
  pid_t forked = fork();
  if (forked == 0) {
    _exit(0);
  }
  if (holder > 0) {
    show("holder", holder);
    kill(holder, SIGKILL);
    wait_for(holder, "holder");
  }
  show("fork returned", forked);
  report("fork waitpid", wait_for(forked, "fork"), forked);

  pid_t vforked = vfork();
  if (vforked == 0) {
    _exit(0);
  }
  show("vfork returned", vforked);
  report("vfork waitpid", wait_for(vforked, "vfork"), vforked);

  /* A raw clone, which the kernel answers through both settid words. */
  static pid_t parent_word, child_word;
  long cloned = syscall(
      SYS_clone,
      CLONE_PARENT_SETTID | CLONE_CHILD_SETTID | SIGCHLD,
      0,
      &parent_word,
#if defined(__aarch64__)
      0,
      &child_word
#else
      &child_word,
      0
#endif
  );
  if (cloned == 0) {
    _exit(child_word == getpid() ? 0 : 4);
  }
  if (cloned < 0) {
    printf("clone failed: %s\n", strerror(errno));
    return 1;
  }
  show("clone returned", cloned);
  report("clone parent_tid word", parent_word, cloned);
  siginfo_t info;
  memset(&info, 0, sizeof info);
  if (waitid(P_PID, (id_t)cloned, &info, WEXITED) != 0) {
    printf("waitid failed: %s\n", strerror(errno));
    return 1;
  }
  report("clone waitid si_pid", info.si_pid, cloned);
  if (info.si_status != 0) {
    printf("  MISMATCH: the clone child's child_tid word was not its pid\n");
    mismatches++;
  }

  /* A raw clone3 of the guest's own. */
  struct clone_args args;
  memset(&args, 0, sizeof args);
  args.exit_signal = SIGCHLD;
  long cloned3 = syscall(SYS_clone3, &args, sizeof args);
  if (cloned3 == 0) {
    _exit(0);
  }
  if (cloned3 < 0) {
    printf("clone3 failed: %s\n", strerror(errno));
    return 1;
  }
  show("clone3 returned", cloned3);
  report("clone3 waitpid", wait_for((pid_t)cloned3, "clone3"), cloned3);

  printf("%d mismatches\n", mismatches);
  return mismatches == 0 ? 0 : 1;
}
