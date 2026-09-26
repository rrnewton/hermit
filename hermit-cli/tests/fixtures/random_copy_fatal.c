/* Real process_vm_writev permission failure; no injected backend error. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

struct shared {
  _Atomic uint32_t root_ready, child_ready, go, child_stop, after_random, child_pid;
};
int main(int argc, char **argv) {
  if (argc != 4) return 90;
  int fd = open(argv[1], O_RDWR);
  if (fd < 0) return 91;
  struct shared *s = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (s == MAP_FAILED) return 92;
  if (close(fd)) return 93;
  int gate = open(argv[3], O_RDONLY);
  if (gate < 0) return 98;
  pid_t child = fork();
  if (child < 0) return 94;
  if (!child) {
    atomic_store(&s->child_pid, (uint32_t)getpid());
    atomic_store(&s->child_ready, 1);
    while (!atomic_load(&s->child_stop)) __asm__ volatile("" ::: "memory");
    _exit(7);
  }
  atomic_store(&s->root_ready, 1);
  unsigned char permit = 0;
  if (read(gate, &permit, 1) != 1 || permit != 1 || !atomic_load(&s->go)) return 99;
  if (close(gate)) return 100;
  if (prctl(PR_SET_DUMPABLE, atoi(argv[2]))) return 95;
  unsigned char bytes[8] = {165,165,165,165,165,165,165,165};
  errno = 0;
  long result = syscall(SYS_getrandom, bytes, 7, 0);
  atomic_store(&s->after_random, 1);
  printf("AFTER_RANDOM result=%ld errno=%d sentinel=%u\n", result, errno, bytes[7]);
  atomic_store(&s->child_stop, 1);
  int status = 0;
  if (waitpid(child, &status, 0) != child) return 96;
  printf("AFTER_CHILD status=%d\n", status);
  return result == 7 && bytes[7] == 165 && WIFEXITED(status) && WEXITSTATUS(status) == 7 ? 0 : 97;
}
