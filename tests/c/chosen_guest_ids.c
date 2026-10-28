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
 * With one of these arguments it makes one call instead and prints its
 * outcome:
 * - `sighand`: a raw clone with CLONE_SIGHAND but not CLONE_VM, which Linux
 *   refuses with EINVAL;
 * - `newpid`: a clone3 with CLONE_NEWPID and no set_tid; the child prints its
 *   own ID, 1 in its namespace;
 * - `newpid-fork`, `newpid-set-tid`: `newpid`, whose child then forks, or
 *   makes a clone3 with CLONE_NEWPID and set_tid [1], from inside its new
 *   namespace;
 * - `own-tid N`: a clone3 with CLONE_NEWPID and its own set_tid of N entries,
 *   [1] (the new namespace's ID only) or [1, first + 100] (also the ID in the
 *   guest's own namespace); the child checks it is 1 in its namespace;
 * - `unknown-flag`: `own-tid 1` with an unknown flag bit too (EINVAL);
 * - `edge-tid`: `own-tid 1` with the array in the last four bytes of a page
 *   that an unmapped page follows;
 * - `kernel-args`, `kernel-args-top`: a clone3 whose struct clone_args
 *   pointer is in the kernel half, (void*)-4096 or (void*)-2 (EFAULT);
 * - `wronly-args`, `wronly-vfork`: a clone3 (with CLONE_VFORK for the
 *   second) whose struct clone_args is on a page mapped writable but not
 *   readable, which the kernel reads all the same; `wronly-tid` the same for
 *   `own-tid 1`'s array; `wronly-short` the first with a size of 32, which
 *   every kernel refuses (EINVAL);
 * - `leader-gone CALL`: CALL, by a second thread once the first has exited
 *   (`pthread_exit`), so that the thread group leader is a zombie;
 * - `vfork-detached`: a raw clone with CLONE_VFORK, CLONE_PIDFD and
 *   CLONE_DETACHED, which every kernel refuses (EINVAL); `untraced-detached`
 *   the same with CLONE_UNTRACED instead of CLONE_VFORK;
 * - `nested`: `own-tid 1` after unshare(CLONE_NEWPID), so that the array is
 *   two levels short of the guest's own namespace;
 * - `no-caps`: `own-tid 1` with CLONE_NEWUSER too, after the guest gives up
 *   every capability: Linux lets it make the user namespace and choose the
 *   ID inside it;
 * - `bad-tid`: a clone3 whose set_tid points at an unmapped address (EFAULT);
 * - `bad-size`: a clone3 with set_tid but set_tid_size 0 (EINVAL);
 * - `seccomp`: a fork after the guest installs a seccomp filter of its own
 *   (one that allows everything);
 * - `signal65`: a raw clone with exit signal 65, which clone accepts and
 *   clone3 does not; `vfork65` the same with CLONE_VFORK, whose parent waits
 *   for its child's exit;
 * - `sp1`: a raw clone with new stack pointer 1, which clone3 cannot express
 *   (its child would fault at once).
 *
 * Every line is the ID's offset from the first process's, so a run whose IDs
 * are chosen from a base prints the same bytes whatever the base, with the
 * offsets in creation order, and a run on host IDs shows whatever IDs other
 * host tasks left. Exits 0 when every ID agreed with the others for the same
 * task, 1 otherwise.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/capability.h>
#include <linux/filter.h>
#include <linux/sched.h>
#include <linux/seccomp.h>
#include <pthread.h>
#include <sched.h>
#include <sys/mman.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
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

/* One call, as the arguments `sighand`, `own-tid N`, `bad-tid` and
 * `bad-size` ask, and its outcome. */
static int one_call(int argc, char** argv) {
  const char* what = argv[1];
  if (strcmp(what, "vfork-detached") == 0 ||
      strcmp(what, "untraced-detached") == 0) {
#ifndef CLONE_DETACHED
#define CLONE_DETACHED 0x00400000
#endif
    int pidfd = -1;
    unsigned long extra =
        strcmp(what, "vfork-detached") == 0 ? CLONE_VFORK : CLONE_UNTRACED;
    long child =
        syscall(SYS_clone, extra | CLONE_PIDFD | CLONE_DETACHED | SIGCHLD, 0,
                &pidfd, 0, 0);
    if (child == 0) {
      syscall(SYS_exit, 0);
    }
    if (child > 0) {
      waitpid((pid_t)child, NULL, __WALL);
    }
    printf("clone %s: %s\n", what, child < 0 ? strerror(errno) : "created");
    return 0;
  }
  if (strcmp(what, "kernel-args") == 0 ||
      strcmp(what, "kernel-args-top") == 0) {
    long pointer = strcmp(what, "kernel-args") == 0 ? -4096L : -2L;
    long child = syscall(SYS_clone3, (void*)pointer, 88);
    printf("clone3 %s: %s\n", what, child < 0 ? strerror(errno) : "created");
    return 0;
  }
  if (strcmp(what, "sighand") == 0) {
    long child = syscall(SYS_clone, CLONE_SIGHAND | SIGCHLD, 0, 0, 0, 0);
    if (child == 0) {
      _exit(0);
    }
    if (child > 0) {
      wait_for((pid_t)child, "clone(CLONE_SIGHAND)");
    }
    printf("clone(CLONE_SIGHAND): %s\n",
           child < 0 ? strerror(errno) : "created");
    return 0;
  }
  if (strcmp(what, "seccomp") == 0) {
    struct sock_filter allow = BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
    struct sock_fprog program = {1, &allow};
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 ||
        syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &program) != 0) {
      printf("seccomp: %s\n", strerror(errno));
      return 1;
    }
    pid_t child = fork();
    if (child == 0) {
      _exit(0);
    }
    wait_for(child, "fork");
    printf("fork after seccomp: child %+ld\n", (long)child - first);
    return 0;
  }
  if (strcmp(what, "signal65") == 0 || strcmp(what, "vfork65") == 0 ||
      strcmp(what, "sp1") == 0) {
    int sp1 = strcmp(what, "sp1") == 0;
    unsigned long flags = sp1 ? SIGCHLD : 65;
    if (strcmp(what, "vfork65") == 0) {
      flags |= CLONE_VFORK;
    }
    long child = syscall(SYS_clone, flags, sp1 ? 1 : 0, 0, 0, 0);
    if (child == 0) {
      syscall(SYS_exit, 0);
    }
    if (child > 0) {
      waitpid((pid_t)child, NULL, __WALL);
    }
    printf("clone %s: %s\n", what, child < 0 ? strerror(errno) : "created");
    return 0;
  }
  pid_t tids[2] = {1, first + 100};
  struct clone_args args;
  memset(&args, 0, sizeof args);
  args.exit_signal = SIGCHLD;
  args.set_tid = (uint64_t)(uintptr_t)tids;
  struct clone_args* argp = &args;
  int nested_call = 0;
  if (strncmp(what, "newpid", 6) == 0) {
    args.flags = CLONE_NEWPID;
    args.set_tid = 0;
    nested_call = strcmp(what, "newpid-fork") == 0   ? 1
                  : strcmp(what, "newpid-set-tid") == 0 ? 2
                                                        : 0;
  } else if (strcmp(what, "own-tid") == 0 && argc > 2) {
    args.flags = CLONE_NEWPID;
    args.set_tid_size = strtoul(argv[2], NULL, 10);
  } else if (strcmp(what, "unknown-flag") == 0) {
    args.flags = CLONE_NEWPID | (1ULL << 40);
    args.set_tid_size = 1;
  } else if (strcmp(what, "nested") == 0) {
    if (unshare(CLONE_NEWPID) != 0) {
      printf("unshare: %s\n", strerror(errno));
      return 1;
    }
    args.flags = CLONE_NEWPID;
    args.set_tid_size = 1;
  } else if (strcmp(what, "no-caps") == 0) {
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct none[2];
    memset(none, 0, sizeof none);
    if (syscall(SYS_capset, &header, none) != 0) {
      printf("capset: %s\n", strerror(errno));
      return 1;
    }
    args.flags = CLONE_NEWUSER | CLONE_NEWPID;
    args.set_tid_size = 1;
  } else if (strcmp(what, "bad-tid") == 0) {
    args.set_tid = 8;
    args.set_tid_size = 1;
  } else if (strcmp(what, "bad-size") == 0) {
    args.set_tid_size = 0;
  } else if (strcmp(what, "edge-tid") == 0) {
    long page = sysconf(_SC_PAGESIZE);
    char* pages = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (pages == MAP_FAILED || munmap(pages + page, page) != 0) {
      printf("mmap: %s\n", strerror(errno));
      return 1;
    }
    pid_t* edge = (pid_t*)(pages + page - sizeof(pid_t));
    *edge = 1;
    args.flags = CLONE_NEWPID;
    args.set_tid = (uint64_t)(uintptr_t)edge;
    args.set_tid_size = 1;
  } else if (strcmp(what, "wronly-tid") == 0) {
    pid_t* wronly = mmap(NULL, sysconf(_SC_PAGESIZE), PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (wronly == MAP_FAILED) {
      printf("mmap: %s\n", strerror(errno));
      return 1;
    }
    *wronly = 1;
    args.flags = CLONE_NEWPID;
    args.set_tid = (uint64_t)(uintptr_t)wronly;
    args.set_tid_size = 1;
  } else if (strcmp(what, "wronly-args") == 0 ||
             strcmp(what, "wronly-vfork") == 0 ||
             strcmp(what, "wronly-short") == 0) {
    args.set_tid = 0;
    if (strcmp(what, "wronly-vfork") == 0) {
      args.flags = CLONE_VFORK;
    }
    argp = mmap(NULL, sysconf(_SC_PAGESIZE), PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (argp == MAP_FAILED) {
      printf("mmap: %s\n", strerror(errno));
      return 1;
    }
    *argp = args;
  } else {
    printf("unknown call %s\n", what);
    return 2;
  }
  size_t size = strcmp(what, "wronly-short") == 0 ? 32 : sizeof args;
  long child = syscall(SYS_clone3, argp, size);
  if (child == 0) {
    if (args.set_tid == 0 && args.flags == CLONE_NEWPID) {
      printf("newpid child: own ID %ld\n", (long)getpid());
    }
    if (nested_call != 0) {
      pid_t inner_tids[1] = {1};
      struct clone_args inner;
      memset(&inner, 0, sizeof inner);
      inner.exit_signal = SIGCHLD;
      if (nested_call == 2) {
        inner.flags = CLONE_NEWPID;
        inner.set_tid = (uint64_t)(uintptr_t)inner_tids;
        inner.set_tid_size = 1;
      }
      long grandchild = syscall(SYS_clone3, &inner, sizeof inner);
      if (grandchild == 0) {
        _exit(0);
      }
      if (grandchild > 0) {
        waitpid((pid_t)grandchild, NULL, 0);
      }
      printf("newpid child's clone3: %s\n",
             grandchild < 0 ? strerror(errno) : "created");
    }
    _exit(getpid() == 1 ? 0 : 1);
  }
  if (child < 0) {
    printf("clone3 %s: %s\n", what, strerror(errno));
    return 0;
  }
  int status = 0;
  waitpid((pid_t)child, &status, 0);
  printf("clone3 %s: child %+ld, exit %d\n", what, child - first,
         WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}

struct deferred_call {
  int argc;
  char** argv;
};

/* Waits until the thread group leader has exited, then makes the call and
 * ends the process with its status. */
static void* call_after_the_leader(void* arg) {
  struct deferred_call* call = arg;
  char path[64];
  snprintf(path, sizeof path, "/proc/self/task/%d/stat", first);
  for (;;) {
    char stat[512] = {0};
    FILE* file = fopen(path, "r");
    if (file == NULL) {
      break;
    }
    size_t got = fread(stat, 1, sizeof stat - 1, file);
    fclose(file);
    char* state = strrchr(stat, ')');
    if (got == 0 || state == NULL || state[1] == '\0' || state[2] == 'Z' ||
        state[2] == 'X') {
      break;
    }
    sched_yield();
  }
  exit(one_call(call->argc, call->argv));
}

int main(int argc, char** argv) {
  int collide = argc > 1 && strcmp(argv[1], "collide") == 0;
  setvbuf(stdout, NULL, _IONBF, 0);
  first = getpid();
  if (argc > 2 && strcmp(argv[1], "leader-gone") == 0) {
    static struct deferred_call call;
    call.argc = argc - 1;
    call.argv = argv + 1;
    pthread_t thread;
    if (pthread_create(&thread, NULL, call_after_the_leader, &call) != 0) {
      printf("pthread_create failed\n");
      return 1;
    }
    pthread_exit(NULL);
  }
  if (argc > 1 && !collide) {
    return one_call(argc, argv);
  }
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
