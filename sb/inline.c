#include <unistd.h>
static long raw(long n, long a, long b, long c) {
  long r; __asm__ volatile("syscall" : "=a"(r) : "a"(n),"D"(a),"S"(b),"d"(c) : "rcx","r11","memory");
  return r;
}
int main(void){ for(int i=0;i<5;i++) raw(39,0,0,0); raw(1,1,(long)"x\n",2); return 0; }
