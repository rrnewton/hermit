#include <stdio.h>

__asm__(".text\n"
        ".p2align 4\n"
        ".global hermit_liteinst_session_getpid\n"
        ".type hermit_liteinst_session_getpid,@function\n"
        "hermit_liteinst_session_getpid:\n"
        "mov $39, %eax\n"
        "syscall\n"
        "nop\n"
        "nop\n"
        "nop\n"
        "ret\n"
        ".size hermit_liteinst_session_getpid, "
        ".-hermit_liteinst_session_getpid\n");

extern long hermit_liteinst_session_getpid(void);
int main(void) {
  long expected = -1;
  for (unsigned int index = 0; index < 32; ++index) {
    long observed = hermit_liteinst_session_getpid();
    if (expected == -1) {
      expected = observed;
    } else if (expected != observed) {
      return 20;
    }
  }
  puts("getpid-ok");
  return 0;
}
