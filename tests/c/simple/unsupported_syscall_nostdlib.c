// @lint-ignore LICENSELINT

#define ENOSYS 38
#define SYS_getpid 39
#define SYS_write 1
#define SYS_exit 60

typedef unsigned long long int uint64;

long syscall0(long number) {
  long result;
  asm volatile("syscall"
               : "=a"(result)
               : "a"(number)
               : "rcx", "r11", "memory");
  return result;
}

long write(int fd, const char* buf, int length) {
  long result;
  asm volatile("syscall"
               : "=a"(result)
               : "a"((uint64)SYS_write),
                 "D"((uint64)fd),
                 "S"((uint64)buf),
                 "d"((uint64)length)
               : "rcx", "r11", "memory");
  return result;
}

_Noreturn void exit(int code) {
  asm volatile("syscall"
               :
               : "a"((uint64)SYS_exit), "D"((uint64)code)
               : "rcx", "r11", "memory");
  __builtin_unreachable();
}

void _start(void) {
  const char blocked[] = "blocked\n";
  const char passed[] = "passed\n";
  long first = syscall0(SYS_getpid);
  long second = syscall0(SYS_getpid);

  if (first == -ENOSYS && second == -ENOSYS) {
    write(1, blocked, sizeof(blocked) - 1);
  } else {
    write(1, passed, sizeof(passed) - 1);
  }
  exit(0);
}
