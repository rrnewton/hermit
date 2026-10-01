typedef unsigned long u64;
static long sys3(long n, long a, long b, long c) {
  long r;
  __asm__ __volatile__("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c)
                       : "rcx", "r11", "memory");
  return r;
}
struct ts { long s; long ns; };
void _start(void) {
  static const char msg[] = "sleeping\n";
  sys3(1, 1, (long)msg, sizeof(msg) - 1);   /* write */
  struct ts t = {30, 0};
  sys3(35, (long)&t, 0, 0);                  /* nanosleep */
  sys3(1, 1, (long)msg, sizeof(msg) - 1);
  sys3(60, 0, 0, 0);                         /* exit */
  __builtin_unreachable();
}
