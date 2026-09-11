#include "acquire.h"

long ma_raw(long number, long first, long second, long third,
            long fourth, long fifth, long sixth) {
  register long reg10 __asm__("r10") = fourth;
  register long reg8 __asm__("r8") = fifth;
  register long reg9 __asm__("r9") = sixth;
  long result;
  __asm__ volatile ("syscall" : "=a"(result) : "a"(number), "D"(first),
                    "S"(second), "d"(third), "r"(reg10), "r"(reg8),
                    "r"(reg9) : "rcx", "r11", "memory");
  return result;
}
