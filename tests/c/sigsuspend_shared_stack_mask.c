// tests/c/sigsuspend_shared_mask_rewrite.c's scenario with the mask on a
// shared stack, just below its red zone: the parent calls the raw rt_sigsuspend
// with its stack pointer at the top of a MAP_SHARED region and its mask 144
// bytes below it, below the red zone, where Hermit used to place its private
// copy of the mask. A peer process rewrites that mask to block
// SIGCHLD after Hermit has read it and before the call runs, while a child's
// exit sends the parent the SIGCHLD the mask let through; the peer then sends
// SIGUSR1.
//
// Linux copies the mask when rt_sigsuspend starts. Under Hermit's schedule the
// rewrite lands after the call started, so the first call wakes for the
// child's SIGCHLD and the second for the peer's SIGUSR1: "sigchld=1
// sigusr1=1". Natively the rewrite may land first, and SIGCHLD then stays
// blocked (sigchld=0). Exits 0 for either, and for nothing else.
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define SHARED_STACK_SIZE (64 * 1024)

struct shared {
  volatile int suspending;
  // The stack the parent's rt_sigsuspend runs on; its signal handlers run on
  // it too.
  char stack[SHARED_STACK_SIZE] __attribute__((aligned(16)));
};

static volatile sig_atomic_t sigchld_count;
static volatile sig_atomic_t sigusr1_count;

static void on_sigchld(int sig) {
  (void)sig;
  sigchld_count++;
}

static void on_sigusr1(int sig) {
  (void)sig;
  sigusr1_count++;
}

// The mask's offset below the shared stack's top.
#define MASK_OFFSET 144

// rt_sigsuspend(top - offset, 8) with the stack pointer at `top`, and the
// stack pointer restored afterwards.
static long sigsuspend_on(char* top, long offset) {
  long result;
  __asm__ volatile(
      "mov %%rsp, %%r12\n\t"
      "mov %[top], %%rsp\n\t"
      "mov %%rsp, %%rdi\n\t"
      "sub %[offset], %%rdi\n\t"
      "mov $8, %%esi\n\t"
      "mov %[nr], %%eax\n\t"
      "syscall\n\t"
      "mov %%r12, %%rsp\n\t"
      : "=a"(result)
      : [top] "r"(top), [offset] "r"(offset), [nr] "i"(SYS_rt_sigsuspend)
      : "rdi", "rsi", "rcx", "r11", "r12", "memory");
  return result;
}

static int reap(pid_t pid) {
  int status;
  if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    fprintf(stderr, "child %d did not exit cleanly\n", (int)pid);
    return 1;
  }
  return 0;
}

int main(void) {
  long offset = MASK_OFFSET;
  struct shared* shared = mmap(
      NULL,
      sizeof(*shared),
      PROT_READ | PROT_WRITE,
      MAP_SHARED | MAP_ANONYMOUS,
      -1,
      0);
  if (shared == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
  char* top = shared->stack + SHARED_STACK_SIZE;
  volatile uint64_t* mask = (volatile uint64_t*)(top - offset);
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_sigchld;
  if (sigaction(SIGCHLD, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  action.sa_handler = on_sigusr1;
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  sigset_t all;
  sigfillset(&all);
  sigprocmask(SIG_SETMASK, &all, NULL);
  pid_t parent = getpid();

  pid_t peer = fork();
  if (peer < 0) {
    perror("fork");
    return 1;
  }
  if (peer == 0) {
    while (!shared->suspending)
      sched_yield();
    sched_yield();
    *mask |= 1ULL << (SIGCHLD - 1);
    for (int i = 0; i < 3; i++)
      sched_yield();
    kill(parent, SIGUSR1);
    _exit(0);
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    while (!shared->suspending)
      sched_yield();
    _exit(0);
  }

  // A mask that blocks nothing, at the shared slot the peer rewrites.
  *mask = 0;
  shared->suspending = 1;
  sched_yield();
  while (!sigusr1_count) {
    long result = sigsuspend_on(top, offset);
    if (result != -4) {
      fprintf(stderr, "rt_sigsuspend returned %ld\n", result);
      return 1;
    }
  }
  if (reap(peer) || reap(child))
    return 1;
  printf("sigchld=%d sigusr1=%d\n", (int)sigchld_count, (int)sigusr1_count);
  return sigchld_count <= 1 && sigusr1_count == 1 ? 0 : 1;
}
