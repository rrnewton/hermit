// @lint-ignore LICENSELINT

#define ENOSYS 38
#define KEYCTL_GET_KEYRING_ID 0
#define KEY_SPEC_THREAD_KEYRING -1
#define SYS_keyctl 250
#define SYS_write 1
#define SYS_exit 60

typedef unsigned long long int uint64;

long syscall3(long number, long arg1, long arg2, long arg3) {
  long result;
  asm volatile("syscall"
               : "=a"(result)
               : "a"((uint64)number),
                 "D"((uint64)arg1),
                 "S"((uint64)arg2),
                 "d"((uint64)arg3)
               : "rcx", "r11", "memory");
  return result;
}

long write(int fd, const char* buf, int length) {
  return syscall3(SYS_write, fd, (long)buf, length);
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
  long result = syscall3(
      SYS_keyctl, KEYCTL_GET_KEYRING_ID, KEY_SPEC_THREAD_KEYRING, 0);

  if (result == -ENOSYS) {
    write(1, blocked, sizeof(blocked) - 1);
  } else {
    write(1, passed, sizeof(passed) - 1);
  }
  exit(0);
}
